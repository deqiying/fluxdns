//! 业务 SQLite `StorageBackend` 首轮 adapter。

use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, Sqlite, SqlitePool};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::dns::{CancelReason, Deadline};
use crate::ports::storage::{
    ResolveRuleSource, SchemaVersion, StatsBatch, StorageBackend, StorageFlushSummary,
    StorageHealth, StorageOperation, StorageTransaction,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::STORAGE_SCHEMA_VERSION;
use super::resolve_log::ResolveDetailRecord;

const SQLITE_BUSY_TIMEOUT_MS: u64 = 2_000;
const INITIAL_STORAGE_SCHEMA_VERSION: SchemaVersion = SchemaVersion(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SqliteResolveDetailWriterBuildError {
    #[error("sqlite resolve detail queue capacity must be greater than zero")]
    ZeroCapacity,
    #[error("sqlite resolve detail batch size must be greater than zero")]
    ZeroBatchSize,
    #[error("sqlite resolve detail eviction threshold must be less than max records")]
    InvalidLimits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteResolveDetailLimits {
    pub eviction_threshold_records: u64,
    pub max_records: u64,
    pub max_record_age: Duration,
}

impl SqliteResolveDetailLimits {
    pub fn new(
        eviction_threshold_records: u64,
        max_records: u64,
        max_record_age: Duration,
    ) -> Result<Self, SqliteResolveDetailWriterBuildError> {
        if eviction_threshold_records == 0
            || eviction_threshold_records >= max_records
            || max_record_age.is_zero()
        {
            return Err(SqliteResolveDetailWriterBuildError::InvalidLimits);
        }
        Ok(Self {
            eviction_threshold_records,
            max_records,
            max_record_age,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SqliteResolveDetailFlushSummary {
    pub committed: u64,
    pub evicted: u64,
    pub dropped: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SqliteResolveDetailRunSummary {
    pub flush: SqliteResolveDetailFlushSummary,
    pub failed_flushes: u64,
}

/// 将有界详情记录送入独立 SQLite writer channel。
#[derive(Clone)]
pub struct SqliteResolveDetailWriter {
    sender: mpsc::Sender<ResolveDetailRecord>,
}

/// SQLite 详情 writer 的受管 flush 端；不在 DNS 请求线程执行数据库 I/O。
pub struct SqliteResolveDetailWorker {
    backend: Arc<SqliteStorageBackend>,
    receiver: mpsc::Receiver<ResolveDetailRecord>,
    pending: VecDeque<ResolveDetailRecord>,
    max_batch: usize,
    limits: Option<SqliteResolveDetailLimits>,
}

impl SqliteResolveDetailWriter {
    pub fn channel(
        backend: Arc<SqliteStorageBackend>,
        capacity: usize,
        max_batch: usize,
    ) -> Result<(Self, SqliteResolveDetailWorker), SqliteResolveDetailWriterBuildError> {
        if capacity == 0 {
            return Err(SqliteResolveDetailWriterBuildError::ZeroCapacity);
        }
        if max_batch == 0 {
            return Err(SqliteResolveDetailWriterBuildError::ZeroBatchSize);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self { sender },
            SqliteResolveDetailWorker {
                backend,
                receiver,
                pending: VecDeque::new(),
                max_batch,
                limits: None,
            },
        ))
    }

    pub fn channel_with_limits(
        backend: Arc<SqliteStorageBackend>,
        capacity: usize,
        max_batch: usize,
        limits: SqliteResolveDetailLimits,
    ) -> Result<(Self, SqliteResolveDetailWorker), SqliteResolveDetailWriterBuildError> {
        SqliteResolveDetailLimits::new(
            limits.eviction_threshold_records,
            limits.max_records,
            limits.max_record_age,
        )?;
        let (writer, mut worker) = Self::channel(backend, capacity, max_batch)?;
        worker.limits = Some(limits);
        Ok((writer, worker))
    }

    /// 由详情 projector 无等待提交一条已经完成校验和裁剪的记录。
    pub(crate) fn try_write(&self, record: ResolveDetailRecord) -> Result<(), PortError> {
        self.sender.try_send(record).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => PortError::new(
                PortErrorClass::ResourceExhausted,
                "sqlite_resolve_log.enqueue",
            )
            .with_safe_context("queue full"),
            mpsc::error::TrySendError::Closed(_) => {
                PortError::new(PortErrorClass::Unavailable, "sqlite_resolve_log.enqueue")
                    .with_safe_context("worker closed")
            }
        })
    }
}

impl SqliteResolveDetailWorker {
    pub fn pending_len(&self) -> usize {
        self.pending.len().saturating_add(self.receiver.len())
    }

    pub async fn flush(
        &mut self,
        deadline: Deadline,
    ) -> Result<SqliteResolveDetailFlushSummary, PortError> {
        while self.pending.len() < self.max_batch {
            match self.receiver.try_recv() {
                Ok(record) => self.pending.push_back(record),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
        if self.pending.is_empty() {
            return Ok(SqliteResolveDetailFlushSummary::default());
        }
        let records = self
            .pending
            .iter()
            .take(self.max_batch)
            .cloned()
            .collect::<Vec<_>>();
        let summary = self
            .backend
            .write_detail_records(records, self.limits, deadline)
            .await?;
        for _ in 0..summary.committed.saturating_add(summary.dropped) {
            let _ = self.pending.pop_front();
        }
        Ok(summary)
    }

    pub async fn shutdown(
        mut self,
        deadline: Deadline,
    ) -> Result<SqliteResolveDetailFlushSummary, PortError> {
        self.receiver.close();
        let mut total = SqliteResolveDetailFlushSummary::default();
        while self.pending_len() > 0 {
            let summary = self.flush(deadline).await?;
            total.committed = total.committed.saturating_add(summary.committed);
            total.evicted = total.evicted.saturating_add(summary.evicted);
            total.dropped = total.dropped.saturating_add(summary.dropped);
        }
        Ok(total)
    }

    pub async fn run(
        self,
        cancellation: crate::dns::Cancellation,
        flush_interval: Duration,
        operation_timeout: Duration,
    ) -> Result<SqliteResolveDetailRunSummary, PortError> {
        let (worker, mut summary) = self
            .run_until_stopped(cancellation, flush_interval, operation_timeout)
            .await?;
        let final_flush = worker
            .shutdown(Deadline::new(Instant::now() + operation_timeout))
            .await?;
        merge_detail_run_flush(&mut summary, Ok(final_flush));
        Ok(summary)
    }

    /// 取消时只结束当前批次并关闭接收端，将剩余队列交还 owner；不抢先排空详情。
    pub(crate) async fn run_until_stopped(
        mut self,
        cancellation: crate::dns::Cancellation,
        flush_interval: Duration,
        operation_timeout: Duration,
    ) -> Result<(Self, SqliteResolveDetailRunSummary), PortError> {
        if flush_interval.is_zero() || operation_timeout.is_zero() {
            return Err(
                PortError::new(PortErrorClass::InvalidInput, "sqlite_resolve_log.run")
                    .with_safe_context("flush interval and operation timeout must be positive"),
            );
        }
        let mut summary = SqliteResolveDetailRunSummary::default();
        let mut interval = tokio::time::interval(flush_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `interval` 的首次 tick 立即完成；先消费它，避免 worker 启动时做一次空 flush。
        interval.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                record = self.receiver.recv() => {
                    let Some(record) = record else { break };
                    self.pending.push_back(record);
                    while self.pending.len() < self.max_batch {
                        match self.receiver.try_recv() {
                            Ok(record) => self.pending.push_back(record),
                            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }
                    if self.pending.len() >= self.max_batch {
                        merge_detail_run_flush(
                            &mut summary,
                            self.flush(Deadline::new(Instant::now() + operation_timeout)).await,
                        );
                    }
                }
                _ = interval.tick() => merge_detail_run_flush(
                    &mut summary,
                    self.flush(Deadline::new(Instant::now() + operation_timeout)).await,
                ),
            }
        }
        self.receiver.close();
        Ok((self, summary))
    }
}

fn merge_detail_run_flush(
    summary: &mut SqliteResolveDetailRunSummary,
    flush: Result<SqliteResolveDetailFlushSummary, PortError>,
) {
    match flush {
        Ok(flush) => {
            summary.flush.committed = summary.flush.committed.saturating_add(flush.committed);
            summary.flush.evicted = summary.flush.evicted.saturating_add(flush.evicted);
            summary.flush.dropped = summary.flush.dropped.saturating_add(flush.dropped);
        }
        Err(_) => summary.failed_flushes = summary.failed_flushes.saturating_add(1),
    }
}

#[derive(Clone)]
pub struct SqliteStorageBackend {
    pool: SqlitePool,
    path: Arc<PathBuf>,
    state: Arc<Mutex<SqliteStorageState>>,
    operation_lock: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    injected_fault: Arc<Mutex<Option<InjectedSqliteFault>>>,
}

#[derive(Clone, Copy)]
struct SqliteStorageState {
    health: StorageHealth,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum InjectedSqliteFault {
    Busy,
    DiskFull,
}

#[cfg(test)]
impl InjectedSqliteFault {
    const fn safe_context(self) -> &'static str {
        match self {
            Self::Busy => "injected busy",
            Self::DiskFull => "injected disk full",
        }
    }
}

impl Default for SqliteStorageState {
    fn default() -> Self {
        Self {
            health: StorageHealth::Healthy,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SqliteStorageBackendBuildError {
    #[error("sqlite storage startup deadline exceeded")]
    Timeout,
    #[error("sqlite storage directory could not be prepared")]
    Directory,
    #[error("sqlite storage database could not be opened")]
    Connect,
    #[error("sqlite storage schema could not be initialized")]
    Schema,
}

impl SqliteStorageBackend {
    /// 打开业务 SQLite，并执行当前版本 migration。
    pub async fn connect(path: impl Into<PathBuf>) -> Result<Self, SqliteStorageBackendBuildError> {
        Self::connect_with_deadline(
            path,
            Deadline::new(Instant::now() + super::service::DEFAULT_STORAGE_OPERATION_TIMEOUT),
        )
        .await
    }

    /// 连接、首次建库和升级共享启动预算；成功前不创建任何业务 writer。
    pub async fn connect_with_deadline(
        path: impl Into<PathBuf>,
        deadline: Deadline,
    ) -> Result<Self, SqliteStorageBackendBuildError> {
        if deadline.is_expired(Instant::now()) {
            return Err(SqliteStorageBackendBuildError::Timeout);
        }
        tokio::time::timeout(
            deadline.remaining(Instant::now()),
            Self::connect_within_budget(path.into(), deadline),
        )
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Timeout)?
    }

    async fn connect_within_budget(
        path: PathBuf,
        deadline: Deadline,
    ) -> Result<Self, SqliteStorageBackendBuildError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|_| SqliteStorageBackendBuildError::Directory)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(
                Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS)
                    .min(deadline.remaining(Instant::now())),
            );
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .acquire_timeout(deadline.remaining(Instant::now()))
            .connect_with(options)
            .await
            .map_err(|_| SqliteStorageBackendBuildError::Connect)?;
        // 首次建表与 metadata 属于一个事务，避免共享启动预算中断后留下半套 schema。
        let mut initialization = pool
            .begin()
            .await
            .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
        let has_meta = sqlx::query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'storage_meta' LIMIT 1",
        )
        .fetch_optional(&mut *initialization)
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Schema)?
        .is_some();
        if !has_meta {
            for statement in include_str!("../../migrations/0001_storage.sql").split(';') {
                let statement = statement.trim();
                if !statement.is_empty() {
                    sqlx::query(statement)
                        .execute(&mut *initialization)
                        .await
                        .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
                }
            }
            sqlx::query(
                "INSERT INTO storage_meta \
                 (singleton, schema_version, database_id, created_at_utc, migrated_at_utc) \
                 VALUES (1, ?, ?, ?, ?)",
            )
            .bind(i64::from(INITIAL_STORAGE_SCHEMA_VERSION.0))
            .bind(format!("fluxdns-{}", std::process::id()))
            .bind(
                system_time_utc_millis(SystemTime::now(), "sqlite_storage.initialize")
                    .map_err(|_| SqliteStorageBackendBuildError::Schema)?,
            )
            .bind(
                system_time_utc_millis(SystemTime::now(), "sqlite_storage.initialize")
                    .map_err(|_| SqliteStorageBackendBuildError::Schema)?,
            )
            .execute(&mut *initialization)
            .await
            .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
        }
        initialization
            .commit()
            .await
            .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
        migrate_storage_schema(&pool).await?;
        Ok(Self {
            pool,
            path: Arc::new(path),
            state: Arc::new(Mutex::new(SqliteStorageState::default())),
            operation_lock: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            injected_fault: Arc::new(Mutex::new(None)),
        })
    }

    /// 在真实目标库执行最小元数据写入并回滚；不产生伪造业务记录或持久化探针数据。
    pub(crate) async fn startup_write_probe(&self, deadline: Deadline) -> Result<(), PortError> {
        let operation = "sqlite_storage.startup_write_probe";
        run_with_deadline(deadline, operation, async {
            let _guard = self.lock_operation(deadline, operation).await?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|error| self.database_error(error, operation))?;
            let changed = sqlx::query(
                "UPDATE storage_meta SET migrated_at_utc_millis = \
                 CASE WHEN migrated_at_utc_millis = 0 THEN 1 ELSE migrated_at_utc_millis - 1 END \
                 WHERE singleton = 1",
            )
            .execute(&mut *transaction)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            if changed.rows_affected() != 1 {
                return Err(PortError::new(PortErrorClass::CorruptData, operation)
                    .with_safe_context("startup metadata row missing"));
            }
            transaction
                .rollback()
                .await
                .map_err(|error| self.database_error(error, operation))
        })
        .await
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    #[cfg(test)]
    fn inject_fault(&self, fault: InjectedSqliteFault) {
        *self
            .injected_fault
            .lock()
            .expect("sqlite injected fault lock must not be poisoned") = Some(fault);
    }

    #[cfg(test)]
    fn take_injected_fault(&self) -> Option<InjectedSqliteFault> {
        self.injected_fault
            .lock()
            .expect("sqlite injected fault lock must not be poisoned")
            .take()
    }

    fn available(&self, operation: &'static str) -> Result<(), PortError> {
        let state = self
            .state
            .lock()
            .map_err(|_| PortError::new(PortErrorClass::Internal, operation))?;
        match state.health {
            StorageHealth::Healthy | StorageHealth::Degraded => Ok(()),
            StorageHealth::Failed | StorageHealth::Stopping => {
                Err(PortError::new(PortErrorClass::Unavailable, operation))
            }
        }
    }

    fn mark_degraded(&self) {
        if let Ok(mut state) = self.state.lock()
            && matches!(
                state.health,
                StorageHealth::Healthy | StorageHealth::Degraded
            )
        {
            state.health = StorageHealth::Degraded;
        }
    }

    fn mark_failed(&self) {
        if let Ok(mut state) = self.state.lock()
            && state.health != StorageHealth::Stopping
        {
            state.health = StorageHealth::Failed;
        }
    }

    fn mark_healthy(&self) {
        if let Ok(mut state) = self.state.lock()
            && state.health == StorageHealth::Degraded
        {
            state.health = StorageHealth::Healthy;
        }
    }

    /// 在 deadline 内取得串行 operation lock，避免数据库排队越过调用方预算。
    async fn lock_operation(
        &self,
        deadline: Deadline,
        operation: &'static str,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>, PortError> {
        let now = Instant::now();
        if deadline.is_expired(now) {
            return Err(PortError::new(PortErrorClass::Timeout, operation));
        }
        tokio::time::timeout(deadline.remaining(now), self.operation_lock.lock())
            .await
            .map_err(|_| PortError::new(PortErrorClass::Timeout, operation))
    }

    async fn migrate_now(
        &self,
        target: SchemaVersion,
        deadline: Deadline,
    ) -> Result<SchemaVersion, PortError> {
        check_deadline(deadline, "sqlite_storage.migrate")?;
        self.available("sqlite_storage.migrate")?;
        if target != STORAGE_SCHEMA_VERSION {
            return Err(
                PortError::new(PortErrorClass::InvalidInput, "sqlite_storage.migrate")
                    .with_safe_context("unsupported schema version"),
            );
        }
        let _guard = self
            .lock_operation(deadline, "sqlite_storage.migrate")
            .await?;
        let row = sqlx::query("SELECT schema_version FROM storage_meta WHERE singleton = 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.migrate"))?;
        let version = row
            .try_get::<i64, _>("schema_version")
            .map_err(|_| PortError::new(PortErrorClass::CorruptData, "sqlite_storage.migrate"))?;
        if version != i64::from(STORAGE_SCHEMA_VERSION.0) {
            return Err(
                PortError::new(PortErrorClass::Unavailable, "sqlite_storage.migrate")
                    .with_safe_context("schema version mismatch"),
            );
        }
        self.mark_healthy();
        Ok(target)
    }

    async fn execute_now(
        &self,
        transaction: StorageTransaction,
        deadline: Deadline,
    ) -> Result<(), PortError> {
        check_deadline(deadline, "sqlite_storage.execute")?;
        self.available("sqlite_storage.execute")?;
        if transaction.idempotency_key.is_empty() || transaction.operations.is_empty() {
            return Err(
                PortError::new(PortErrorClass::InvalidInput, "sqlite_storage.execute")
                    .with_safe_context("empty transaction"),
            );
        }
        let _guard = self
            .lock_operation(deadline, "sqlite_storage.execute")
            .await?;
        let mut sql_transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.execute"))?;
        #[cfg(test)]
        let mut injected_fault = self.take_injected_fault();
        for operation in transaction.operations {
            let result = match operation {
                StorageOperation::StatsBatch(batch) => {
                    apply_stats_batch(&mut sql_transaction, &batch).await
                }
                StorageOperation::ResolveBatch(batch) => {
                    apply_resolve_batch(&mut sql_transaction, &batch).await
                }
            };
            #[cfg(test)]
            let result = match injected_fault.take() {
                Some(fault) => Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "sqlite_storage.execute",
                )
                .with_safe_context(fault.safe_context())),
                None => result,
            };
            if let Err(error) = result {
                if matches!(error.class(), PortErrorClass::Unavailable) {
                    self.mark_degraded();
                }
                return Err(error);
            }
            check_deadline(deadline, "sqlite_storage.execute")?;
        }
        sql_transaction
            .commit()
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.execute"))?;
        self.mark_healthy();
        Ok(())
    }

    async fn write_detail_records(
        &self,
        records: Vec<ResolveDetailRecord>,
        limits: Option<SqliteResolveDetailLimits>,
        deadline: Deadline,
    ) -> Result<SqliteResolveDetailFlushSummary, PortError> {
        run_with_deadline(deadline, "sqlite_storage.resolve_detail", async move {
            self.available("sqlite_storage.resolve_detail")?;
            if records.is_empty() {
                return Ok(SqliteResolveDetailFlushSummary::default());
            }
            let _guard = self
                .lock_operation(deadline, "sqlite_storage.resolve_detail")
                .await?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|error| self.database_error(error, "sqlite_storage.resolve_detail"))?;
            let summary =
                match apply_resolve_records_with_limits(&mut transaction, &records, limits).await {
                    Ok(summary) => summary,
                    Err(error) => {
                        if matches!(error.class(), PortErrorClass::Unavailable) {
                            self.mark_degraded();
                        }
                        return Err(error);
                    }
                };
            check_deadline(deadline, "sqlite_storage.resolve_detail")?;
            transaction
                .commit()
                .await
                .map_err(|error| self.database_error(error, "sqlite_storage.resolve_detail"))?;
            self.mark_healthy();
            Ok(summary)
        })
        .await
    }

    async fn checkpoint_now(&self, deadline: Deadline) -> Result<(), PortError> {
        check_deadline(deadline, "sqlite_storage.checkpoint")?;
        self.available("sqlite_storage.checkpoint")?;
        let _guard = self
            .lock_operation(deadline, "sqlite_storage.checkpoint")
            .await?;
        sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(&self.pool)
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.checkpoint"))?;
        self.mark_healthy();
        Ok(())
    }

    fn database_error(&self, error: sqlx::Error, operation: &'static str) -> PortError {
        match error {
            sqlx::Error::Io(_) | sqlx::Error::Database(_) => {
                self.mark_degraded();
                PortError::new(PortErrorClass::Unavailable, operation)
            }
            sqlx::Error::PoolClosed => {
                self.mark_failed();
                PortError::new(PortErrorClass::Unavailable, operation)
            }
            _ => {
                self.mark_failed();
                PortError::new(PortErrorClass::Internal, operation)
            }
        }
    }
}

/// 按版本顺序执行小步 migration；每个版本在同一事务中更新 schema 标记。
async fn migrate_storage_schema(pool: &SqlitePool) -> Result<(), SqliteStorageBackendBuildError> {
    let row = sqlx::query("SELECT schema_version FROM storage_meta WHERE singleton = 1")
        .fetch_one(pool)
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
    let version = row
        .try_get::<i64, _>("schema_version")
        .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
    let mut version = u32::try_from(version).map_err(|_| SqliteStorageBackendBuildError::Schema)?;
    while version < STORAGE_SCHEMA_VERSION.0 {
        let (next, migration) = match version {
            1 => (
                2,
                include_str!("../../migrations/0002_resolution_metadata.sql"),
            ),
            2 => (
                3,
                include_str!("../../migrations/0003_management_query_projection.sql"),
            ),
            3 => (
                4,
                include_str!("../../migrations/0004_query_record_observability.sql"),
            ),
            4 => (
                5,
                include_str!("../../migrations/0005_dns_core_duration.sql"),
            ),
            5 => (
                6,
                include_str!("../../migrations/0006_integer_business_timestamps.sql"),
            ),
            _ => return Err(SqliteStorageBackendBuildError::Schema),
        };
        apply_storage_migration(pool, version, next, migration).await?;
        version = next;
    }
    if version == STORAGE_SCHEMA_VERSION.0 {
        Ok(())
    } else {
        Err(SqliteStorageBackendBuildError::Schema)
    }
}

async fn apply_storage_migration(
    pool: &SqlitePool,
    from: u32,
    to: u32,
    migration: &'static str,
) -> Result<(), SqliteStorageBackendBuildError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
    for statement in migration.split(';') {
        let statement = statement.trim();
        if !statement.is_empty() {
            sqlx::query(statement)
                .execute(&mut *transaction)
                .await
                .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
        }
    }
    // v6 重建了 metadata 时间列，旧版本之间仍使用历史列名完成前向升级。
    let update_sql = if to >= 6 {
        "UPDATE storage_meta SET schema_version = ?, migrated_at_utc_millis = ? \
         WHERE singleton = 1 AND schema_version = ?"
    } else {
        "UPDATE storage_meta SET schema_version = ?, migrated_at_utc = ? \
         WHERE singleton = 1 AND schema_version = ?"
    };
    let update = sqlx::query(update_sql)
        .bind(i64::from(to))
        .bind(
            system_time_utc_millis(SystemTime::now(), "sqlite_storage.migrate")
                .map_err(|_| SqliteStorageBackendBuildError::Schema)?,
        )
        .bind(i64::from(from))
        .execute(&mut *transaction)
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
    if update.rows_affected() != 1 {
        return Err(SqliteStorageBackendBuildError::Schema);
    }
    transaction
        .commit()
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Schema)
}

impl std::fmt::Debug for SqliteStorageBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteStorageBackend")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// 将完整 SQLite 操作限制在调用方 deadline 内，并保留稳定的超时分类。
async fn run_with_deadline<T>(
    deadline: Deadline,
    operation: &'static str,
    future: impl std::future::Future<Output = Result<T, PortError>>,
) -> Result<T, PortError> {
    let now = Instant::now();
    if deadline.is_expired(now) {
        return Err(PortError::new(PortErrorClass::Timeout, operation));
    }
    tokio::time::timeout(deadline.remaining(now), future)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Timeout, operation))?
}

impl StorageBackend for SqliteStorageBackend {
    fn migrate(
        &self,
        target: SchemaVersion,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<SchemaVersion, PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_storage.migrate",
            self.migrate_now(target, deadline),
        ))
    }

    fn execute(
        &self,
        transaction: StorageTransaction,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_storage.execute",
            self.execute_now(transaction, deadline),
        ))
    }

    fn health_probe(&self, deadline: Deadline) -> PortFuture<'_, Result<StorageHealth, PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_storage.health_probe",
            async move {
                let state_health = {
                    let state = self.state.lock().map_err(|_| {
                        PortError::new(PortErrorClass::Internal, "sqlite_storage.health_probe")
                    })?;
                    state.health
                };
                if matches!(
                    state_health,
                    StorageHealth::Stopping | StorageHealth::Failed
                ) {
                    return Ok(state_health);
                }
                match sqlx::query("SELECT 1").execute(&self.pool).await {
                    Ok(_) => {
                        if let Ok(mut state) = self.state.lock() {
                            state.health = StorageHealth::Healthy;
                        }
                        Ok(StorageHealth::Healthy)
                    }
                    Err(error) => Err(self.database_error(error, "sqlite_storage.health_probe")),
                }
            },
        ))
    }

    fn checkpoint(&self, deadline: Deadline) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_storage.checkpoint",
            self.checkpoint_now(deadline),
        ))
    }

    fn flush(&self, deadline: Deadline) -> PortFuture<'_, Result<StorageFlushSummary, PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_storage.flush",
            async move {
                self.checkpoint_now(deadline).await?;
                Ok(StorageFlushSummary::default())
            },
        ))
    }

    fn shutdown(
        &self,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<StorageFlushSummary, PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_storage.shutdown",
            async move {
                let _guard = self
                    .lock_operation(deadline, "sqlite_storage.shutdown")
                    .await?;
                if let Ok(mut state) = self.state.lock() {
                    if state.health == StorageHealth::Stopping {
                        return Ok(StorageFlushSummary::default());
                    }
                    state.health = StorageHealth::Stopping;
                }
                sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                    .execute(&self.pool)
                    .await
                    .map_err(|error| self.database_error(error, "sqlite_storage.shutdown"))?;
                self.pool.close().await;
                Ok(StorageFlushSummary::default())
            },
        ))
    }
}

async fn apply_stats_batch(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    batch: &StatsBatch,
) -> Result<(), PortError> {
    if batch.batch_id == 0 || batch.events.is_empty() {
        return Err(
            PortError::new(PortErrorClass::InvalidInput, "sqlite_storage.stats_batch")
                .with_safe_context("empty or invalid batch"),
        );
    }
    let fingerprint = stats_fingerprint(batch);
    if let Some(row) = sqlx::query(
        "SELECT max_event_seq, counter_epoch, payload_hash \
         FROM stats_batch_ledger WHERE batch_id = ?",
    )
    .bind(i64::try_from(batch.batch_id).map_err(|_| {
        PortError::new(
            PortErrorClass::ResourceExhausted,
            "sqlite_storage.stats_batch",
        )
    })?)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| PortError::new(PortErrorClass::Unavailable, "sqlite_storage.stats_batch"))?
    {
        let stored_max = row.try_get::<i64, _>("max_event_seq").unwrap_or_default();
        let stored_epoch = row.try_get::<i64, _>("counter_epoch").unwrap_or_default();
        let stored_hash = row
            .try_get::<Vec<u8>, _>("payload_hash")
            .unwrap_or_default();
        if stored_max == i64::try_from(batch.max_event_sequence).unwrap_or(i64::MAX)
            && stored_epoch == i64::try_from(batch.counter_epoch).unwrap_or(i64::MAX)
            && stored_hash == fingerprint.to_be_bytes()
        {
            return Ok(());
        }
        return Err(
            PortError::new(PortErrorClass::CorruptData, "sqlite_storage.stats_batch")
                .with_safe_context("batch payload conflict"),
        );
    }

    let mut sequences = HashSet::with_capacity(batch.events.len());
    let max_sequence = batch
        .events
        .iter()
        .map(crate::ports::storage::StatsEvent::sequence)
        .max()
        .unwrap_or(0);
    if max_sequence != batch.max_event_sequence
        || !batch
            .events
            .iter()
            .all(|event| sequences.insert(event.sequence()))
    {
        return Err(
            PortError::new(PortErrorClass::InvalidInput, "sqlite_storage.stats_batch")
                .with_safe_context("invalid event sequence"),
        );
    }
    for event in &batch.events {
        sqlx::query(
            "INSERT INTO stats_daily_total (day_utc, total_requests) VALUES (?, 1) \
             ON CONFLICT(day_utc) DO UPDATE SET total_requests = total_requests + 1",
        )
        .bind(i64::from(event.day_utc()))
        .execute(&mut **transaction)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "sqlite_storage.stats_batch"))?;
        for dimension in event.dimensions() {
            let (kind, value) = dimension.database_parts();
            sqlx::query(
                "INSERT INTO stats_daily_dimension \
                 (day_utc, dimension_kind, dimension_value, count) VALUES (?, ?, ?, 1) \
                 ON CONFLICT(day_utc, dimension_kind, dimension_value) \
                 DO UPDATE SET count = count + 1",
            )
            .bind(i64::from(event.day_utc()))
            .bind(kind)
            .bind(value)
            .execute(&mut **transaction)
            .await
            .map_err(|_| {
                PortError::new(PortErrorClass::Unavailable, "sqlite_storage.stats_batch")
            })?;
        }
    }
    sqlx::query(
        "INSERT INTO stats_batch_ledger \
         (batch_id, max_event_seq, counter_epoch, committed_at_utc_millis, payload_hash) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(i64::try_from(batch.batch_id).map_err(|_| {
        PortError::new(
            PortErrorClass::ResourceExhausted,
            "sqlite_storage.stats_batch",
        )
    })?)
    .bind(i64::try_from(batch.max_event_sequence).map_err(|_| {
        PortError::new(
            PortErrorClass::ResourceExhausted,
            "sqlite_storage.stats_batch",
        )
    })?)
    .bind(i64::try_from(batch.counter_epoch).map_err(|_| {
        PortError::new(
            PortErrorClass::ResourceExhausted,
            "sqlite_storage.stats_batch",
        )
    })?)
    .bind(system_time_utc_millis(
        SystemTime::now(),
        "sqlite_storage.stats_batch",
    )?)
    .bind(fingerprint.to_be_bytes().to_vec())
    .execute(&mut **transaction)
    .await
    .map_err(|_| PortError::new(PortErrorClass::Unavailable, "sqlite_storage.stats_batch"))?;
    Ok(())
}

async fn apply_resolve_batch(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    batch: &[crate::ports::storage::ResolveEvent],
) -> Result<(), PortError> {
    let records = batch
        .iter()
        .cloned()
        .map(ResolveDetailRecord::from_event)
        .collect::<Result<Vec<_>, _>>()?;
    apply_resolve_records(transaction, &records).await
}

async fn apply_resolve_records(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    records: &[ResolveDetailRecord],
) -> Result<(), PortError> {
    let _ = apply_resolve_records_with_limits(transaction, records, None).await?;
    Ok(())
}

async fn apply_resolve_records_with_limits(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    records: &[ResolveDetailRecord],
    limits: Option<SqliteResolveDetailLimits>,
) -> Result<SqliteResolveDetailFlushSummary, PortError> {
    let (evicted, available) = if let Some(limits) = limits {
        let cutoff = SystemTime::now()
            .checked_sub(limits.max_record_age)
            .unwrap_or(UNIX_EPOCH);
        let age_result = sqlx::query("DELETE FROM resolve_log WHERE event_time_utc_millis < ?")
            .bind(system_time_utc_millis(
                cutoff,
                "sqlite_storage.resolve_detail",
            )?)
            .execute(&mut **transaction)
            .await
            .map_err(|_| {
                PortError::new(PortErrorClass::Unavailable, "sqlite_storage.resolve_detail")
            })?;
        let mut evicted = age_result.rows_affected();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&mut **transaction)
            .await
            .map_err(|_| {
                PortError::new(PortErrorClass::Unavailable, "sqlite_storage.resolve_detail")
            })?;
        if count >= i64::try_from(limits.eviction_threshold_records).unwrap_or(i64::MAX) {
            let keep = limits.eviction_threshold_records.saturating_sub(1);
            let delete_count = u64::try_from(count)
                .unwrap_or(u64::MAX)
                .saturating_sub(keep);
            if delete_count > 0 {
                let result = sqlx::query(
                    "DELETE FROM resolve_log WHERE id IN (\
                     SELECT id FROM resolve_log ORDER BY event_time_utc_millis ASC, id ASC LIMIT ?)",
                )
                .bind(i64::try_from(delete_count).unwrap_or(i64::MAX))
                .execute(&mut **transaction)
                .await
                .map_err(|_| {
                    PortError::new(PortErrorClass::Unavailable, "sqlite_storage.resolve_detail")
                })?;
                evicted = evicted.saturating_add(result.rows_affected());
            }
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&mut **transaction)
            .await
            .map_err(|_| {
                PortError::new(PortErrorClass::Unavailable, "sqlite_storage.resolve_detail")
            })?;
        let available = limits
            .max_records
            .saturating_sub(u64::try_from(count).unwrap_or(u64::MAX));
        (evicted, available)
    } else {
        (0, records.len() as u64)
    };
    let accepted_len = records
        .len()
        .min(usize::try_from(available).unwrap_or(usize::MAX));
    for record in records.iter().take(accepted_len) {
        let duration_millis = i64::try_from(record.duration_millis()).unwrap_or(i64::MAX);
        let dns_core_duration_micros =
            i64::try_from(record.dns_core_duration_micros()).unwrap_or(i64::MAX);
        let request_digest = if record.has_request_digest() {
            "<present>"
        } else {
            "<absent>"
        };
        let route_id = record.has_route().then_some("<present>");
        let client_ip = record.client_ip().map(|value| value.to_string());
        let client_bucket = record.client_bucket();
        let strategy_id = record.strategy_id();
        let upstream_id = record.upstream_id();
        let upstream_member_id = record.upstream_member_id();
        let upstream_used_id = record.upstream_used_id();
        let matched_rule_source = record.matched_rule_source().map(resolve_rule_source_name);
        let matched_resource_id = record.has_matched_resource().then_some("<present>");
        let matched_rule_ordinal = record
            .matched_rule_ordinal()
            .map(|ordinal| i64::try_from(ordinal).unwrap_or(i64::MAX));
        let resource_revision = record
            .resource_version()
            .map(|version| format!("{}:{}", version.epoch(), version.revision()));
        let answer_summary_json = serde_json::to_string(record.answers()).map_err(|_| {
            PortError::new(PortErrorClass::InvalidInput, "sqlite_storage.resolve_batch")
        })?;
        sqlx::query(
            "INSERT INTO resolve_log \
             (event_time_utc_millis, duration_millis, dns_core_duration_micros, request_id_digest, listener_id, route_id, \
               client_bucket, strategy_id, canonical_qname, qtype, qclass, source, upstream_id, \
               upstream_member_id, matched_rule_source, matched_resource_id, matched_rule_ordinal, \
               rcode, cache_status, failure_class, cancellation_reason, runtime_revision, resource_revision, \
               transport, client_ip, upstream_used_id, answer_count, answers_truncated, answer_summary_json) \
              VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(system_time_utc_millis(
            record.occurred_at(),
            "sqlite_storage.resolve_batch",
        )?)
        .bind(duration_millis)
        .bind(dns_core_duration_micros)
        .bind(request_digest)
        .bind(record.listener_id())
        .bind(route_id)
        .bind(client_bucket)
        .bind(strategy_id)
        .bind(record.qname())
        .bind(i64::from(record.qtype()))
        .bind(i64::from(record.qclass()))
        .bind(stats_source_name(record.source()))
        .bind(upstream_id)
        .bind(upstream_member_id)
        .bind(matched_rule_source)
        .bind(matched_resource_id)
        .bind(matched_rule_ordinal)
        .bind(i64::from(record.rcode()))
        .bind(cache_status_name(record.cache_status()))
        .bind(failure_class_name(record.outcome()))
        .bind(record.cancellation_reason().map(cancellation_reason_name))
        .bind(i64::try_from(record.runtime_revision().0).unwrap_or(i64::MAX))
        .bind(resource_revision.as_deref())
        .bind(transport_name(record.transport()))
        .bind(client_ip.as_deref())
        .bind(upstream_used_id)
        .bind(i64::from(record.answer_count()))
        .bind(record.answers_truncated())
        .bind(answer_summary_json)
        .execute(&mut **transaction)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "sqlite_storage.resolve_batch"))?;
    }
    Ok(SqliteResolveDetailFlushSummary {
        committed: accepted_len as u64,
        evicted,
        dropped: records.len().saturating_sub(accepted_len) as u64,
    })
}

fn stats_fingerprint(batch: &StatsBatch) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    batch.batch_id.hash(&mut hasher);
    batch.max_event_sequence.hash(&mut hasher);
    batch.counter_epoch.hash(&mut hasher);
    for event in &batch.events {
        event.sequence().hash(&mut hasher);
        event.day_utc().hash(&mut hasher);
        event.dimensions().hash(&mut hasher);
    }
    hasher.finish()
}

fn cache_status_name(value: crate::ports::telemetry::CacheStatus) -> &'static str {
    match value {
        crate::ports::telemetry::CacheStatus::Disabled => "disabled",
        crate::ports::telemetry::CacheStatus::Miss => "miss",
        crate::ports::telemetry::CacheStatus::Fresh => "fresh",
        crate::ports::telemetry::CacheStatus::Stale => "stale",
        crate::ports::telemetry::CacheStatus::StoreUnavailable => "store_unavailable",
        crate::ports::telemetry::CacheStatus::WriteRejected => "write_rejected",
    }
}

fn stats_source_name(value: crate::ports::storage::StatsSource) -> &'static str {
    match value {
        crate::ports::storage::StatsSource::Cache => "cache",
        crate::ports::storage::StatsSource::Hosts => "hosts",
        crate::ports::storage::StatsSource::RuleSet => "rule_set",
        crate::ports::storage::StatsSource::Upstream => "upstream",
    }
}

fn transport_name(value: crate::dns::TransportClass) -> &'static str {
    match value {
        crate::dns::TransportClass::Datagram => "udp",
        crate::dns::TransportClass::Stream => "tcp",
        crate::dns::TransportClass::Multiplexed => "doh",
    }
}

/// 将规则来源编码为稳定且低基数的 SQLite 文本值。
fn resolve_rule_source_name(value: ResolveRuleSource) -> &'static str {
    match value {
        ResolveRuleSource::ListenerHosts => "listener_hosts",
        ResolveRuleSource::StrategyHosts => "strategy_hosts",
        ResolveRuleSource::RuleSet => "rule_set",
    }
}

/// 将请求终态压缩为详情表的低基数 failure 分类；正常响应和拒绝由 RCODE 表达。
fn failure_class_name(value: crate::ports::telemetry::OutcomeClass) -> Option<&'static str> {
    match value {
        crate::ports::telemetry::OutcomeClass::Success
        | crate::ports::telemetry::OutcomeClass::Rejected => None,
        crate::ports::telemetry::OutcomeClass::Failure => Some("failure"),
        crate::ports::telemetry::OutcomeClass::Timeout => Some("timeout"),
        crate::ports::telemetry::OutcomeClass::Cancelled => Some("cancelled"),
        crate::ports::telemetry::OutcomeClass::Dropped => Some("dropped"),
    }
}

/// 将协作式取消原因编码为稳定的 SQLite 文本值。
fn cancellation_reason_name(value: CancelReason) -> &'static str {
    match value {
        CancelReason::ClientDisconnected => "client_disconnected",
        CancelReason::DeadlineExceeded => "deadline_exceeded",
        CancelReason::Shutdown => "shutdown",
        CancelReason::GroupPolicy => "group_policy",
        CancelReason::UpstreamCancelled => "upstream_cancelled",
    }
}

fn check_deadline(deadline: Deadline, operation: &'static str) -> Result<(), PortError> {
    if deadline.is_expired(Instant::now()) {
        Err(PortError::new(PortErrorClass::Timeout, operation))
    } else {
        Ok(())
    }
}

/// SQLite 业务绝对时间统一为 UTC 毫秒整数，亚毫秒截断，epoch 前仍沿用旧契约归零。
fn system_time_utc_millis(time: SystemTime, operation: &'static str) -> Result<i64, PortError> {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).map_err(|_| {
        PortError::new(PortErrorClass::InvalidInput, operation)
            .with_safe_context("UTC millisecond timestamp exceeds signed 64-bit storage")
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use super::{
        InjectedSqliteFault, SqliteConnectOptions, SqlitePoolOptions, SqliteResolveDetailLimits,
        SqliteResolveDetailWriter, SqliteResolveDetailWriterBuildError, SqliteStorageBackend,
    };
    use crate::dns::{CancelReason, Cancellation, Deadline, RuntimeRevision, TransportClass};
    use crate::ports::storage::{
        ResolveEvent, ResolveRuleSource, StatsBatch, StatsEvent, StatsSource, StorageBackend,
        StorageOperation, StorageTransaction,
    };
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::resource::ResourceVersion;
    use crate::storage::ResolveDetailRecord;
    use sqlx::Row;

    static NEXT_TEST_DB: AtomicU64 = AtomicU64::new(0);

    trait TestResolveEventWriter {
        fn try_record(&self, event: ResolveEvent) -> Result<(), crate::ports::PortError>;
    }

    impl TestResolveEventWriter for SqliteResolveDetailWriter {
        fn try_record(&self, event: ResolveEvent) -> Result<(), crate::ports::PortError> {
            self.try_write(ResolveDetailRecord::from_event(event)?)
        }
    }

    fn path() -> std::path::PathBuf {
        let id = NEXT_TEST_DB.fetch_add(1, Ordering::Relaxed);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/tests/storage");
        std::fs::create_dir_all(&root).unwrap();
        root.join(format!(
            "fluxdns-storage-{id}-{}.sqlite3",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn startup_deadline_rejects_before_creating_database() {
        let path = path();
        let result =
            SqliteStorageBackend::connect_with_deadline(&path, Deadline::new(Instant::now())).await;
        assert!(matches!(
            result,
            Err(super::SqliteStorageBackendBuildError::Timeout)
        ));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn failed_initial_schema_creation_rolls_back_preceding_tables() {
        let path = path();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        // 后续建表冲突用于触发真实 DDL 失败，前面的 storage_meta 不得残留。
        sqlx::query("CREATE TABLE stats_daily_total (marker INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            SqliteStorageBackend::connect(&path).await,
            Err(super::SqliteStorageBackendBuildError::Schema)
        ));
        let meta_tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='storage_meta'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(meta_tables, 0);
        let original_tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='stats_daily_total'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(original_tables, 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_probe_rolls_back_real_write_and_reports_write_failure() {
        let backend = SqliteStorageBackend::connect(path()).await.unwrap();
        let before: (i64, i64) = sqlx::query_as(
            "SELECT schema_version, migrated_at_utc_millis FROM storage_meta WHERE singleton=1",
        )
        .fetch_one(&backend.pool)
        .await
        .unwrap();
        backend.startup_write_probe(deadline()).await.unwrap();
        let after: (i64, i64) = sqlx::query_as(
            "SELECT schema_version, migrated_at_utc_millis FROM storage_meta WHERE singleton=1",
        )
        .fetch_one(&backend.pool)
        .await
        .unwrap();
        assert_eq!(before, after);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stats_daily_total")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        sqlx::query("CREATE TRIGGER reject_probe BEFORE UPDATE ON storage_meta BEGIN SELECT RAISE(ABORT, 'test probe failure'); END")
            .execute(&backend.pool).await.unwrap();
        let error = backend.startup_write_probe(deadline()).await.unwrap_err();
        assert_eq!(error.operation(), "sqlite_storage.startup_write_probe");
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Unavailable
        ));
        sqlx::query("DROP TRIGGER reject_probe")
            .execute(&backend.pool)
            .await
            .unwrap();
        backend.startup_write_probe(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();
    }

    #[tokio::test]
    async fn startup_and_write_probe_obey_shared_budget_under_real_sqlite_lock() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let mut connection = backend.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *connection)
            .await
            .unwrap();
        let started = Instant::now();
        let startup_deadline = Deadline::new(started + Duration::from_millis(30));
        let opened = SqliteStorageBackend::connect_with_deadline(&path, startup_deadline)
            .await
            .unwrap();
        // 当前版本的只读核对可以打开库，可写性仍由同预算内的真实写探针保证。
        assert!(opened.startup_write_probe(startup_deadline).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        let error = backend
            .startup_write_probe(Deadline::new(Instant::now() + Duration::from_millis(20)))
            .await
            .unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        sqlx::query("ROLLBACK")
            .execute(&mut *connection)
            .await
            .unwrap();
        drop(connection);
        opened.shutdown(deadline()).await.unwrap();
        backend.startup_write_probe(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();
    }

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn transaction(batch: StatsBatch) -> StorageTransaction {
        StorageTransaction {
            idempotency_key: "test-batch".into(),
            operations: vec![StorageOperation::StatsBatch(batch)],
        }
    }

    async fn legacy_v5_database() -> (std::path::PathBuf, sqlx::SqlitePool) {
        let path = path();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        for migration in [
            include_str!("../../migrations/0001_storage.sql"),
            include_str!("../../migrations/0002_resolution_metadata.sql"),
            include_str!("../../migrations/0003_management_query_projection.sql"),
            include_str!("../../migrations/0004_query_record_observability.sql"),
            include_str!("../../migrations/0005_dns_core_duration.sql"),
        ] {
            for statement in migration
                .split(';')
                .map(str::trim)
                .filter(|sql| !sql.is_empty())
            {
                sqlx::query(statement).execute(&pool).await.unwrap();
            }
        }
        sqlx::query(
            "INSERT INTO storage_meta VALUES (1, 5, 'timestamp-migration-test', '1000', '2000')",
        )
        .execute(&pool)
        .await
        .unwrap();
        (path, pool)
    }

    async fn insert_legacy_detail(pool: &sqlx::SqlitePool, id: i64, timestamp: &str) {
        sqlx::query(
            "INSERT INTO resolve_log \
             (id, event_time_utc, duration_millis, request_id_digest, listener_id, route_id, \
              client_bucket, strategy_id, canonical_qname, qtype, qclass, source, upstream_id, \
              rcode, cache_status, runtime_revision, resource_revision, upstream_member_id, \
              matched_rule_source, matched_resource_id, matched_rule_ordinal, transport, client_ip, \
              upstream_used_id, answer_count, answers_truncated, answer_summary_json, dns_core_duration_micros) \
             VALUES (?, ?, 8, '<present>', 'listener', 'route', 'client', 'strategy', \
              'example.test.', 1, 1, 'upstream', 'group', 0, 'miss', 7, '2:3', 'member', \
              'rule_set', 'resource', 2, 'udp', '192.0.2.1', 'member', 0, 0, '[]', 250)",
        )
        .bind(id).bind(timestamp).execute(pool).await.unwrap();
    }

    #[test]
    fn business_timestamp_conversion_uses_utc_millis_and_preserves_epoch_boundary() {
        let convert = |time| super::system_time_utc_millis(time, "test.timestamp").unwrap();
        assert_eq!(convert(SystemTime::UNIX_EPOCH), 0);
        assert_eq!(
            convert(SystemTime::UNIX_EPOCH + Duration::from_nanos(1_234_567_890)),
            1_234,
        );
        assert_eq!(
            convert(SystemTime::UNIX_EPOCH - Duration::from_millis(1)),
            0
        );
        if let Some(overflow) =
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(i64::MAX as u64 + 1))
        {
            assert!(super::system_time_utc_millis(overflow, "test.timestamp").is_err());
        }
    }

    #[tokio::test]
    async fn v6_migration_preserves_data_ledger_integer_times_and_sequence_on_reopen() {
        let (path, pool) = legacy_v5_database().await;
        let batch = StatsBatch {
            batch_id: 7,
            max_event_sequence: 1,
            counter_epoch: 2,
            events: vec![StatsEvent::new(1, 20_000, vec![]).unwrap()],
        };
        sqlx::query("INSERT INTO stats_daily_total VALUES (20000, 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO stats_daily_dimension VALUES (20000, 'source', 'upstream', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO stats_batch_ledger VALUES (7, 1, 2, '9223372036854775807', ?)")
            .bind(super::stats_fingerprint(&batch).to_be_bytes().to_vec())
            .execute(&pool)
            .await
            .unwrap();
        for (id, millis) in [
            (1, "0"),
            (2, "999"),
            (3, "1000"),
            (4, "10000000000000"),
            (100, "1"),
        ] {
            insert_legacy_detail(&pool, id, millis).await;
        }
        sqlx::query("DELETE FROM resolve_log WHERE id = 100")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let meta: (i64, String, i64, i64, String, String) = sqlx::query_as(
            "SELECT schema_version, database_id, created_at_utc_millis, migrated_at_utc_millis, \
             typeof(created_at_utc_millis), typeof(migrated_at_utc_millis) FROM storage_meta",
        )
        .fetch_one(&backend.pool)
        .await
        .unwrap();
        assert_eq!(meta.0, 6);
        assert_eq!(meta.1, "timestamp-migration-test");
        assert_eq!(meta.2, 1000);
        assert!(meta.3 > 2000);
        assert_eq!((meta.4.as_str(), meta.5.as_str()), ("integer", "integer"));
        let times: Vec<(i64, i64, String)> = sqlx::query_as(
            "SELECT id, event_time_utc_millis, typeof(event_time_utc_millis) FROM resolve_log ORDER BY id",
        ).fetch_all(&backend.pool).await.unwrap();
        assert_eq!(
            times,
            vec![
                (1, 0, "integer".into()),
                (2, 999, "integer".into()),
                (3, 1000, "integer".into()),
                (4, 10_000_000_000_000, "integer".into())
            ],
        );
        let detail = sqlx::query("SELECT * FROM resolve_log WHERE id = 1")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        for (column, expected) in [
            ("route_id", "route"),
            ("client_bucket", "client"),
            ("strategy_id", "strategy"),
            ("canonical_qname", "example.test."),
            ("upstream_id", "group"),
            ("resource_revision", "2:3"),
            ("upstream_member_id", "member"),
            ("matched_rule_source", "rule_set"),
            ("matched_resource_id", "resource"),
            ("transport", "udp"),
            ("client_ip", "192.0.2.1"),
            ("upstream_used_id", "member"),
            ("answer_summary_json", "[]"),
        ] {
            assert_eq!(detail.get::<String, _>(column), expected, "{column}");
        }
        for (column, expected) in [
            ("duration_millis", 8),
            ("dns_core_duration_micros", 250),
            ("runtime_revision", 7),
            ("matched_rule_ordinal", 2),
            ("answer_count", 0),
            ("answers_truncated", 0),
        ] {
            assert_eq!(detail.get::<i64, _>(column), expected, "{column}");
        }
        let ledger: (i64, String, i64, i64) = sqlx::query_as(
            "SELECT committed_at_utc_millis, typeof(committed_at_utc_millis), max_event_seq, \
             counter_epoch FROM stats_batch_ledger WHERE batch_id = 7",
        )
        .fetch_one(&backend.pool)
        .await
        .unwrap();
        assert_eq!(ledger, (i64::MAX, "integer".into(), 1, 2));
        backend
            .execute(transaction(batch), deadline())
            .await
            .unwrap();
        let total: i64 = sqlx::query_scalar("SELECT total_requests FROM stats_daily_total")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(total, 1, "migrated ledger must keep retries idempotent");
        let dimension: i64 = sqlx::query_scalar("SELECT count FROM stats_daily_dimension")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(dimension, 1);
        let sequence: i64 =
            sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name='resolve_log'")
                .fetch_one(&backend.pool)
                .await
                .unwrap();
        assert_eq!(sequence, 100);
        backend.startup_write_probe(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();

        let reopened = SqliteStorageBackend::connect(&path).await.unwrap();
        let migration_time: i64 =
            sqlx::query_scalar("SELECT migrated_at_utc_millis FROM storage_meta")
                .fetch_one(&reopened.pool)
                .await
                .unwrap();
        assert_eq!(
            migration_time, meta.3,
            "reopen and probe must not change migration time"
        );
        let inserted = sqlx::query(
            "INSERT INTO resolve_log (event_time_utc_millis, duration_millis, request_id_digest, \
             listener_id, canonical_qname, qtype, qclass, rcode, cache_status, runtime_revision) \
             VALUES (1001, 0, '<absent>', 'listener', 'example.test.', 1, 1, 0, 'miss', 1)",
        )
        .execute(&reopened.pool)
        .await
        .unwrap();
        assert_eq!(inserted.last_insert_rowid(), 101);
        reopened.shutdown(deadline()).await.unwrap();
    }

    #[tokio::test]
    async fn v6_migration_keeps_sequence_when_all_details_were_deleted() {
        let (path, pool) = legacy_v5_database().await;
        insert_legacy_detail(&pool, 77, "1000").await;
        sqlx::query("DELETE FROM resolve_log")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let sequence: i64 =
            sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name='resolve_log'")
                .fetch_one(&backend.pool)
                .await
                .unwrap();
        assert_eq!(sequence, 77);
        backend.shutdown(deadline()).await.unwrap();
    }

    #[tokio::test]
    async fn invalid_legacy_timestamps_roll_back_all_v6_schema_changes() {
        for invalid in ["", "not-time", "1.5", "1e3", "9223372036854775808", "-1"] {
            let (path, pool) = legacy_v5_database().await;
            insert_legacy_detail(&pool, 9, invalid).await;
            assert!(matches!(
                SqliteStorageBackend::connect(&path).await,
                Err(super::SqliteStorageBackendBuildError::Schema),
            ));
            let meta: (i64, String, String) = sqlx::query_as(
                "SELECT schema_version, created_at_utc, migrated_at_utc FROM storage_meta",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(meta, (5, "1000".into(), "2000".into()));
            let preserved: String =
                sqlx::query_scalar("SELECT event_time_utc FROM resolve_log WHERE id=9")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(preserved, invalid);
            let artifacts: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN \
                 ('storage_meta_v6', 'stats_batch_ledger_v6', 'resolve_log_v6')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(artifacts, 0);
            let index_sql: String = sqlx::query_scalar(
                "SELECT sql FROM sqlite_master WHERE name='resolve_log_event_time_idx'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!index_sql.contains("millis"));
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn invalid_legacy_metadata_or_ledger_time_cannot_be_silently_coerced() {
        for statement in [
            "UPDATE storage_meta SET created_at_utc = 'bad'",
            "UPDATE storage_meta SET migrated_at_utc = '9223372036854775808'",
            "INSERT INTO stats_batch_ledger VALUES (1, 1, 1, '1.5', X'01')",
        ] {
            let (path, pool) = legacy_v5_database().await;
            sqlx::query(statement).execute(&pool).await.unwrap();
            let before: (i64, String, String) = sqlx::query_as(
                "SELECT schema_version, created_at_utc, migrated_at_utc FROM storage_meta",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(matches!(
                SqliteStorageBackend::connect(&path).await,
                Err(super::SqliteStorageBackendBuildError::Schema),
            ));
            let after: (i64, String, String) = sqlx::query_as(
                "SELECT schema_version, created_at_utc, migrated_at_utc FROM storage_meta",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(before, after);
            let ledger: Vec<String> =
                sqlx::query_scalar("SELECT committed_at_utc FROM stats_batch_ledger")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(ledger.is_empty() || ledger == ["1.5"]);
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn integer_timestamp_constraints_and_numeric_retention_are_enforced() {
        let backend = SqliteStorageBackend::connect(path()).await.unwrap();
        for invalid in ["bad", "1.5", "-1", "9223372036854775808"] {
            assert!(
                sqlx::query("UPDATE storage_meta SET created_at_utc_millis = ?")
                    .bind(invalid)
                    .execute(&backend.pool)
                    .await
                    .is_err()
            );
        }
        let rows = sqlx::query("PRAGMA table_info(storage_meta)")
            .fetch_all(&backend.pool)
            .await
            .unwrap();
        for name in ["created_at_utc_millis", "migrated_at_utc_millis"] {
            assert_eq!(
                rows.iter()
                    .find(|row| row.get::<String, _>("name") == name)
                    .unwrap()
                    .get::<String, _>("type"),
                "INTEGER",
            );
        }
        for millis in [1000_i64, 999, 10_000_000_000_000] {
            sqlx::query(
                "INSERT INTO resolve_log (event_time_utc_millis, duration_millis, request_id_digest, \
                 listener_id, canonical_qname, qtype, qclass, rcode, cache_status, runtime_revision) \
                 VALUES (?, 0, '<absent>', 'listener', 'example.test.', 1, 1, 0, 'miss', 1)",
            ).bind(millis).execute(&backend.pool).await.unwrap();
        }
        let limits = SqliteResolveDetailLimits::new(3, 4, Duration::from_secs(u64::MAX)).unwrap();
        let mut transaction = backend.pool.begin().await.unwrap();
        let summary = super::apply_resolve_records_with_limits(&mut transaction, &[], Some(limits))
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        assert_eq!(summary.evicted, 1);
        let remaining: Vec<i64> = sqlx::query_scalar(
            "SELECT event_time_utc_millis FROM resolve_log ORDER BY event_time_utc_millis",
        )
        .fetch_all(&backend.pool)
        .await
        .unwrap();
        assert_eq!(remaining, vec![1000, 10_000_000_000_000]);
        let limits = SqliteResolveDetailLimits::new(10, 20, Duration::from_secs(1)).unwrap();
        let mut transaction = backend.pool.begin().await.unwrap();
        let summary = super::apply_resolve_records_with_limits(&mut transaction, &[], Some(limits))
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        assert_eq!(summary.evicted, 1);
        let remaining: Vec<i64> =
            sqlx::query_scalar("SELECT event_time_utc_millis FROM resolve_log")
                .fetch_all(&backend.pool)
                .await
                .unwrap();
        assert_eq!(remaining, vec![10_000_000_000_000]);
        backend.shutdown(deadline()).await.unwrap();
    }

    #[tokio::test]
    async fn migrates_commits_and_reopens_idempotent_stats_batch() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        assert_eq!(
            backend
                .migrate(crate::storage::STORAGE_SCHEMA_VERSION, deadline())
                .await
                .unwrap(),
            crate::storage::STORAGE_SCHEMA_VERSION
        );
        let batch = StatsBatch {
            batch_id: 1,
            max_event_sequence: 3,
            counter_epoch: 2,
            events: vec![
                StatsEvent::new(3, 20_260_902, vec![]).unwrap(),
                StatsEvent::new(2, 20_260_902, vec![]).unwrap(),
            ],
        };
        backend
            .execute(transaction(batch.clone()), deadline())
            .await
            .unwrap();
        backend
            .execute(transaction(batch), deadline())
            .await
            .unwrap();
        let total: i64 =
            sqlx::query_scalar("SELECT total_requests FROM stats_daily_total WHERE day_utc = ?")
                .bind(20_260_902_i64)
                .fetch_one(&backend.pool)
                .await
                .unwrap();
        assert_eq!(total, 2);
        let times: (String, String, String, i64) = sqlx::query_as(
            "SELECT typeof(created_at_utc_millis), typeof(migrated_at_utc_millis), \
             typeof(committed_at_utc_millis), committed_at_utc_millis \
             FROM storage_meta CROSS JOIN stats_batch_ledger",
        )
        .fetch_one(&backend.pool)
        .await
        .unwrap();
        assert_eq!(
            (times.0.as_str(), times.1.as_str(), times.2.as_str()),
            ("integer", "integer", "integer")
        );
        assert!(times.3 > 0);
        backend.shutdown(deadline()).await.unwrap();
        let reopened = SqliteStorageBackend::connect(&path).await.unwrap();
        let health = reopened.health_probe(deadline()).await.unwrap();
        assert_eq!(health, crate::ports::storage::StorageHealth::Healthy);
        reopened.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn connect_upgrades_v1_schema_without_backfilling_existing_details() {
        let path = path();
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for statement in include_str!("../../migrations/0001_storage.sql").split(';') {
            let statement = statement.trim();
            if !statement.is_empty() {
                sqlx::query(statement).execute(&pool).await.unwrap();
            }
        }
        sqlx::query(
            "INSERT INTO storage_meta \
             (singleton, schema_version, database_id, created_at_utc, migrated_at_utc) \
             VALUES (1, 1, 'v1-test', '0', '0')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO resolve_log \
             (event_time_utc, duration_millis, request_id_digest, listener_id, canonical_qname, \
              qtype, qclass, source, upstream_id, rcode, cache_status, runtime_revision) \
             VALUES ('0', 0, '<present>', 'listener', 'len:0', 1, 1, 'upstream', \
                     '<present>', 0, 'miss', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let version: i64 =
            sqlx::query_scalar("SELECT schema_version FROM storage_meta WHERE singleton = 1")
                .fetch_one(&backend.pool)
                .await
                .unwrap();
        assert_eq!(version, i64::from(crate::storage::STORAGE_SCHEMA_VERSION.0));
        let row = sqlx::query(
            "SELECT upstream_member_id, matched_rule_source, matched_resource_id, \
              matched_rule_ordinal, transport, client_ip, upstream_used_id, answer_count, \
              answers_truncated, answer_summary_json, dns_core_duration_micros \
              FROM resolve_log LIMIT 1",
        )
        .fetch_one(&backend.pool)
        .await
        .unwrap();
        assert_eq!(
            row.try_get::<Option<String>, _>("upstream_member_id")
                .unwrap(),
            None
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("matched_rule_source")
                .unwrap(),
            None
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("matched_resource_id")
                .unwrap(),
            None
        );
        for column in ["client_ip", "upstream_used_id", "answer_summary_json"] {
            assert_eq!(row.try_get::<Option<String>, _>(column).unwrap(), None);
        }
        assert_eq!(row.try_get::<Option<i64>, _>("answer_count").unwrap(), None);
        assert_eq!(
            row.try_get::<Option<i64>, _>("answers_truncated").unwrap(),
            None
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("matched_rule_ordinal")
                .unwrap(),
            None
        );
        assert_eq!(row.try_get::<Option<String>, _>("transport").unwrap(), None);
        assert_eq!(
            row.try_get::<Option<i64>, _>("dns_core_duration_micros")
                .unwrap(),
            None
        );
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn degraded_backend_recovers_after_successful_operation() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        backend
            .state
            .lock()
            .expect("sqlite state lock must not be poisoned")
            .health = crate::ports::storage::StorageHealth::Degraded;

        let batch = StatsBatch {
            batch_id: 99,
            max_event_sequence: 1,
            counter_epoch: 0,
            events: vec![StatsEvent::new(1, 20_260_902, vec![]).unwrap()],
        };
        backend
            .execute(transaction(batch), deadline())
            .await
            .unwrap();
        assert_eq!(
            backend.health_probe(deadline()).await.unwrap(),
            crate::ports::storage::StorageHealth::Healthy
        );

        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn failed_backend_does_not_auto_recover_from_probe() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        backend
            .state
            .lock()
            .expect("sqlite state lock must not be poisoned")
            .health = crate::ports::storage::StorageHealth::Failed;

        assert_eq!(
            backend.health_probe(deadline()).await.unwrap(),
            crate::ports::storage::StorageHealth::Failed
        );
        assert!(backend.checkpoint(deadline()).await.is_err());

        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn injected_busy_and_disk_full_faults_degrade_then_recover() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();

        for (fault, batch_id) in [
            (InjectedSqliteFault::Busy, 77),
            (InjectedSqliteFault::DiskFull, 78),
        ] {
            backend.inject_fault(fault);
            let failed_batch = StatsBatch {
                batch_id,
                max_event_sequence: batch_id,
                counter_epoch: 0,
                events: vec![StatsEvent::new(batch_id, 20_260_902, vec![]).unwrap()],
            };
            let error = backend
                .execute(transaction(failed_batch), deadline())
                .await
                .unwrap_err();
            assert!(matches!(
                error.class(),
                crate::ports::PortErrorClass::Unavailable
            ));
            assert_eq!(
                backend
                    .state
                    .lock()
                    .expect("sqlite state lock must not be poisoned")
                    .health,
                crate::ports::storage::StorageHealth::Degraded
            );

            let recovered_batch = StatsBatch {
                batch_id: batch_id + 100,
                max_event_sequence: batch_id + 100,
                counter_epoch: 0,
                events: vec![StatsEvent::new(batch_id + 100, 20_260_902, vec![]).unwrap()],
            };
            backend
                .execute(transaction(recovered_batch), deadline())
                .await
                .unwrap();
            assert_eq!(
                backend
                    .state
                    .lock()
                    .expect("sqlite state lock must not be poisoned")
                    .health,
                crate::ports::storage::StorageHealth::Healthy
            );
        }

        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// 通过真实 SQLite 写锁验证 Busy 会降级，并在锁释放后的成功事务中恢复。
    #[tokio::test]
    async fn real_sqlite_write_lock_degrades_then_recovers() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let mut lock_connection = backend.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *lock_connection)
            .await
            .unwrap();

        let locked_batch = StatsBatch {
            batch_id: 79,
            max_event_sequence: 79,
            counter_epoch: 0,
            events: vec![StatsEvent::new(79, 20_260_902, vec![]).unwrap()],
        };
        let error = backend
            .execute(transaction(locked_batch), deadline())
            .await
            .unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Unavailable
        ));
        assert_eq!(
            backend
                .state
                .lock()
                .expect("sqlite state lock must not be poisoned")
                .health,
            crate::ports::storage::StorageHealth::Degraded
        );

        sqlx::query("ROLLBACK")
            .execute(&mut *lock_connection)
            .await
            .unwrap();
        drop(lock_connection);
        let recovered_batch = StatsBatch {
            batch_id: 80,
            max_event_sequence: 80,
            counter_epoch: 0,
            events: vec![StatsEvent::new(80, 20_260_902, vec![]).unwrap()],
        };
        backend
            .execute(transaction(recovered_batch), deadline())
            .await
            .unwrap();
        assert_eq!(
            backend
                .state
                .lock()
                .expect("sqlite state lock must not be poisoned")
                .health,
            crate::ports::storage::StorageHealth::Healthy
        );

        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// 验证业务 SQLite 操作和 shutdown 在等待串行锁时遵守 deadline。
    #[tokio::test]
    async fn operation_lock_wait_honors_deadline() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let guard = backend.operation_lock.lock().await;

        let batch = StatsBatch {
            batch_id: 81,
            max_event_sequence: 81,
            counter_epoch: 0,
            events: vec![StatsEvent::new(81, 20_260_902, vec![]).unwrap()],
        };
        let execute_deadline = Deadline::new(Instant::now() + Duration::from_millis(20));
        let execute_error = backend
            .execute(transaction(batch), execute_deadline)
            .await
            .unwrap_err();
        assert!(matches!(
            execute_error.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        assert_eq!(execute_error.operation(), "sqlite_storage.execute");

        let shutdown_deadline = Deadline::new(Instant::now() + Duration::from_millis(20));
        let shutdown_error = backend.shutdown(shutdown_deadline).await.unwrap_err();
        assert!(matches!(
            shutdown_error.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        assert_eq!(shutdown_error.operation(), "sqlite_storage.shutdown");

        drop(guard);
        let recovered_batch = StatsBatch {
            batch_id: 82,
            max_event_sequence: 82,
            counter_epoch: 0,
            events: vec![StatsEvent::new(82, 20_260_902, vec![]).unwrap()],
        };
        backend
            .execute(transaction(recovered_batch), deadline())
            .await
            .unwrap();
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// 验证连接池排队不会越过更短的调用方 deadline。
    #[tokio::test]
    async fn sqlite_pool_wait_honors_short_caller_deadline() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let mut connections = Vec::new();
        for _ in 0..4 {
            connections.push(backend.pool.acquire().await.unwrap());
        }

        let batch = StatsBatch {
            batch_id: 83,
            max_event_sequence: 83,
            counter_epoch: 0,
            events: vec![StatsEvent::new(83, 20_260_902, vec![]).unwrap()],
        };
        let short_deadline = Deadline::new(Instant::now() + Duration::from_millis(20));
        let error = backend
            .execute(transaction(batch), short_deadline)
            .await
            .unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        assert_eq!(error.operation(), "sqlite_storage.execute");
        assert_eq!(
            backend
                .state
                .lock()
                .expect("sqlite state lock must not be poisoned")
                .health,
            crate::ports::storage::StorageHealth::Healthy
        );

        drop(connections);
        let recovered_batch = StatsBatch {
            batch_id: 84,
            max_event_sequence: 84,
            counter_epoch: 0,
            events: vec![StatsEvent::new(84, 20_260_902, vec![]).unwrap()],
        };
        backend
            .execute(transaction(recovered_batch), deadline())
            .await
            .unwrap();
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn transaction_rolls_back_on_invalid_batch_and_shutdown_is_terminal() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let invalid = StatsBatch {
            batch_id: 2,
            max_event_sequence: 9,
            counter_epoch: 1,
            events: vec![StatsEvent::new(8, 20_260_902, vec![]).unwrap()],
        };
        assert!(
            backend
                .execute(transaction(invalid), deadline())
                .await
                .is_err()
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stats_batch_ledger")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        backend.shutdown(deadline()).await.unwrap();
        assert!(
            backend
                .health_probe(deadline())
                .await
                .is_ok_and(|health| { health == crate::ports::storage::StorageHealth::Stopping })
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn resolve_detail_batch_is_written_in_the_same_database_boundary() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        let transaction = StorageTransaction {
            idempotency_key: "detail-batch".into(),
            operations: vec![StorageOperation::ResolveBatch(vec![ResolveEvent {
                occurred_at: SystemTime::UNIX_EPOCH + Duration::from_micros(1_234_567),
                duration_millis: 8,
                dns_core_duration_micros: 250,
                request_digest: Arc::from("digest"),
                listener_id: Arc::from("listener"),
                route_id: Some(Arc::from("route")),
                client_ip: Some("192.0.2.10".parse().unwrap()),
                client_bucket: None,
                strategy_id: Some(Arc::from("strategy")),
                upstream_id: Some(Arc::from("upstream")),
                upstream_member_id: Some(Arc::from("member")),
                upstream_used_id: Some(Arc::from("member")),
                matched_rule_source: Some(ResolveRuleSource::RuleSet),
                matched_resource_id: Some(Arc::from("rules")),
                matched_rule_ordinal: Some(2),
                resource_version: Some(ResourceVersion::new(2, 1)),
                transport: TransportClass::Datagram,
                qname: Arc::from("example.com."),
                qtype: 1,
                qclass: 1,
                answers: vec![crate::ports::storage::ResolveAnswer {
                    name: "example.com.".to_owned(),
                    record_type: "A".to_owned(),
                    data: "192.0.2.20".to_owned(),
                    ttl: 60,
                }],
                rcode: 2,
                cancellation_reason: Some(CancelReason::Shutdown),
                outcome: OutcomeClass::Failure,
                source: StatsSource::Upstream,
                cache_status: CacheStatus::Miss,
                runtime_revision: RuntimeRevision(3),
            }])],
        };
        backend.execute(transaction, deadline()).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let row = sqlx::query(
            "SELECT event_time_utc_millis, typeof(event_time_utc_millis) AS event_time_type, \
             duration_millis, dns_core_duration_micros, request_id_digest, route_id, client_bucket, strategy_id, upstream_id, \
             upstream_member_id, matched_rule_source, matched_resource_id, matched_rule_ordinal, \
             canonical_qname, source, rcode, failure_class, cancellation_reason, resource_revision, \
             transport, client_ip, upstream_used_id, answer_count, answers_truncated, answer_summary_json \
             FROM resolve_log LIMIT 1",
        )
        .fetch_one(&backend.pool)
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>("event_time_utc_millis"), 1234);
        assert_eq!(row.get::<String, _>("event_time_type"), "integer");
        assert_eq!(row.try_get::<i64, _>("duration_millis").unwrap(), 8);
        assert_eq!(
            row.try_get::<Option<i64>, _>("dns_core_duration_micros")
                .unwrap(),
            Some(250)
        );
        assert_eq!(
            row.try_get::<String, _>("request_id_digest").unwrap(),
            "<present>"
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("route_id")
                .unwrap()
                .as_deref(),
            Some("<present>")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("client_bucket")
                .unwrap()
                .as_deref(),
            None
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("strategy_id")
                .unwrap()
                .as_deref(),
            Some("strategy")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("upstream_id")
                .unwrap()
                .as_deref(),
            Some("upstream")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("upstream_member_id")
                .unwrap()
                .as_deref(),
            Some("member")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("matched_rule_source")
                .unwrap()
                .as_deref(),
            Some("rule_set")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("matched_resource_id")
                .unwrap()
                .as_deref(),
            Some("<present>")
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("matched_rule_ordinal")
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            row.try_get::<String, _>("canonical_qname").unwrap(),
            "example.com."
        );
        assert_eq!(row.try_get::<String, _>("source").unwrap(), "upstream");
        assert_eq!(row.try_get::<i64, _>("rcode").unwrap(), 2);
        assert_eq!(
            row.try_get::<Option<String>, _>("failure_class")
                .unwrap()
                .as_deref(),
            Some("failure")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("cancellation_reason")
                .unwrap()
                .as_deref(),
            Some("shutdown")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("resource_revision")
                .unwrap()
                .as_deref(),
            Some("2:1")
        );
        assert_eq!(row.try_get::<String, _>("transport").unwrap(), "udp");
        assert_eq!(
            row.try_get::<Option<String>, _>("client_ip")
                .unwrap()
                .as_deref(),
            Some("192.0.2.10")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("upstream_used_id")
                .unwrap()
                .as_deref(),
            Some("member")
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("answer_count").unwrap(),
            Some(1)
        );
        assert_eq!(
            row.try_get::<Option<bool>, _>("answers_truncated").unwrap(),
            Some(false)
        );
        let answer_summary = row
            .try_get::<Option<String>, _>("answer_summary_json")
            .unwrap()
            .unwrap();
        let answer_summary: serde_json::Value = serde_json::from_str(&answer_summary).unwrap();
        assert_eq!(answer_summary[0]["data"], "192.0.2.20");
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn resolve_log_writer_flushes_through_bounded_sqlite_worker_batches() {
        let path = path();
        let backend = Arc::new(SqliteStorageBackend::connect(&path).await.unwrap());
        let (sink, mut worker) =
            SqliteResolveDetailWriter::channel(Arc::clone(&backend), 2, 1).unwrap();
        let writer = sink;
        for listener_id in ["listener-a", "listener-b"] {
            writer
                .try_record(ResolveEvent {
                    occurred_at: SystemTime::now(),
                    duration_millis: 8,
                    dns_core_duration_micros: 250,
                    request_digest: Arc::from("digest"),
                    listener_id: Arc::from(listener_id),
                    route_id: None,
                    client_ip: None,
                    client_bucket: None,
                    strategy_id: None,
                    upstream_id: None,
                    upstream_member_id: None,
                    upstream_used_id: None,
                    matched_rule_source: None,
                    matched_resource_id: None,
                    matched_rule_ordinal: None,
                    resource_version: None,
                    transport: TransportClass::Datagram,
                    qname: Arc::from("example.com."),
                    qtype: 1,
                    qclass: 1,
                    answers: Vec::new(),
                    rcode: 0,
                    cancellation_reason: None,
                    outcome: OutcomeClass::Success,
                    source: StatsSource::Upstream,
                    cache_status: CacheStatus::Miss,
                    runtime_revision: RuntimeRevision(1),
                })
                .unwrap();
        }
        assert_eq!(worker.pending_len(), 2);
        assert_eq!(worker.flush(deadline()).await.unwrap().committed, 1);
        assert_eq!(worker.pending_len(), 1);
        assert_eq!(worker.flush(deadline()).await.unwrap().committed, 1);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn sqlite_detail_writer_rejects_zero_queue_or_batch_capacity() {
        let path = path();
        let backend = Arc::new(SqliteStorageBackend::connect(&path).await.unwrap());
        assert!(matches!(
            SqliteResolveDetailWriter::channel(Arc::clone(&backend), 0, 1),
            Err(SqliteResolveDetailWriterBuildError::ZeroCapacity)
        ));
        assert!(matches!(
            SqliteResolveDetailWriter::channel(Arc::clone(&backend), 1, 0),
            Err(SqliteResolveDetailWriterBuildError::ZeroBatchSize)
        ));
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn sqlite_detail_worker_retains_batch_when_backend_flush_fails() {
        let path = path();
        let backend = Arc::new(SqliteStorageBackend::connect(&path).await.unwrap());
        let (sink, mut worker) =
            SqliteResolveDetailWriter::channel(Arc::clone(&backend), 1, 1).unwrap();
        let writer = sink;
        writer
            .try_record(ResolveEvent {
                occurred_at: SystemTime::now(),
                duration_millis: 8,
                dns_core_duration_micros: 250,
                request_digest: Arc::from("digest"),
                listener_id: Arc::from("listener"),
                route_id: None,
                client_ip: None,
                client_bucket: None,
                strategy_id: None,
                upstream_id: None,
                upstream_member_id: None,
                upstream_used_id: None,
                matched_rule_source: None,
                matched_resource_id: None,
                matched_rule_ordinal: None,
                resource_version: None,
                transport: TransportClass::Datagram,
                qname: Arc::from("example.com."),
                qtype: 1,
                qclass: 1,
                answers: Vec::new(),
                rcode: 0,
                cancellation_reason: None,
                outcome: OutcomeClass::Success,
                source: StatsSource::Upstream,
                cache_status: CacheStatus::Miss,
                runtime_revision: RuntimeRevision(1),
            })
            .unwrap();
        backend.shutdown(deadline()).await.unwrap();
        assert!(worker.flush(deadline()).await.is_err());
        assert_eq!(worker.pending_len(), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn sqlite_detail_worker_enforces_hard_record_limits() {
        let path = path();
        let backend = Arc::new(SqliteStorageBackend::connect(&path).await.unwrap());
        let limits = SqliteResolveDetailLimits::new(2, 3, Duration::from_secs(3_600)).unwrap();
        let (sink, mut worker) =
            SqliteResolveDetailWriter::channel_with_limits(Arc::clone(&backend), 4, 4, limits)
                .unwrap();
        let writer = sink;
        for listener_id in ["listener-a", "listener-b", "listener-c", "listener-d"] {
            writer
                .try_record(ResolveEvent {
                    occurred_at: SystemTime::now(),
                    duration_millis: 8,
                    dns_core_duration_micros: 250,
                    request_digest: Arc::from("digest"),
                    listener_id: Arc::from(listener_id),
                    route_id: None,
                    client_ip: None,
                    client_bucket: None,
                    strategy_id: None,
                    upstream_id: None,
                    upstream_member_id: None,
                    upstream_used_id: None,
                    matched_rule_source: None,
                    matched_resource_id: None,
                    matched_rule_ordinal: None,
                    resource_version: None,
                    transport: TransportClass::Datagram,
                    qname: Arc::from("example.com."),
                    qtype: 1,
                    qclass: 1,
                    answers: Vec::new(),
                    rcode: 0,
                    cancellation_reason: None,
                    outcome: OutcomeClass::Success,
                    source: StatsSource::Upstream,
                    cache_status: CacheStatus::Miss,
                    runtime_revision: RuntimeRevision(1),
                })
                .unwrap();
        }
        let first = worker.flush(deadline()).await.unwrap();
        assert_eq!(first.committed, 3);
        assert_eq!(first.dropped, 1);
        assert_eq!(first.evicted, 0);

        writer
            .try_record(ResolveEvent {
                occurred_at: SystemTime::now(),
                duration_millis: 8,
                dns_core_duration_micros: 250,
                request_digest: Arc::from("digest"),
                listener_id: Arc::from("listener-e"),
                route_id: None,
                client_ip: None,
                client_bucket: None,
                strategy_id: None,
                upstream_id: None,
                upstream_member_id: None,
                upstream_used_id: None,
                matched_rule_source: None,
                matched_resource_id: None,
                matched_rule_ordinal: None,
                resource_version: None,
                transport: TransportClass::Datagram,
                qname: Arc::from("example.com."),
                qtype: 1,
                qclass: 1,
                answers: Vec::new(),
                rcode: 0,
                cancellation_reason: None,
                outcome: OutcomeClass::Success,
                source: StatsSource::Upstream,
                cache_status: CacheStatus::Miss,
                runtime_revision: RuntimeRevision(1),
            })
            .unwrap();
        let second = worker.flush(deadline()).await.unwrap();
        assert_eq!(second.committed, 1);
        assert_eq!(second.dropped, 0);
        assert_eq!(second.evicted, 2);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn sqlite_detail_worker_evicts_records_older_than_max_age() {
        let path = path();
        let backend = Arc::new(SqliteStorageBackend::connect(&path).await.unwrap());
        let limits = SqliteResolveDetailLimits::new(2, 3, Duration::from_secs(3_600)).unwrap();
        let (sink, mut worker) =
            SqliteResolveDetailWriter::channel_with_limits(Arc::clone(&backend), 2, 2, limits)
                .unwrap();
        let writer = sink;
        for (listener_id, occurred_at) in [
            ("listener-old", SystemTime::UNIX_EPOCH),
            ("listener-new", SystemTime::now()),
        ] {
            writer
                .try_record(ResolveEvent {
                    occurred_at,
                    duration_millis: 8,
                    dns_core_duration_micros: 250,
                    request_digest: Arc::from("digest"),
                    listener_id: Arc::from(listener_id),
                    route_id: None,
                    client_ip: None,
                    client_bucket: None,
                    strategy_id: None,
                    upstream_id: None,
                    upstream_member_id: None,
                    upstream_used_id: None,
                    matched_rule_source: None,
                    matched_resource_id: None,
                    matched_rule_ordinal: None,
                    resource_version: None,
                    transport: TransportClass::Datagram,
                    qname: Arc::from("example.com."),
                    qtype: 1,
                    qclass: 1,
                    answers: Vec::new(),
                    rcode: 0,
                    cancellation_reason: None,
                    outcome: OutcomeClass::Success,
                    source: StatsSource::Upstream,
                    cache_status: CacheStatus::Miss,
                    runtime_revision: RuntimeRevision(1),
                })
                .unwrap();
            let summary = worker.flush(deadline()).await.unwrap();
            if occurred_at == SystemTime::UNIX_EPOCH {
                assert_eq!(summary.evicted, 0);
            } else {
                assert_eq!(summary.evicted, 1);
            }
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn sqlite_detail_worker_shutdown_drains_all_pending_batches() {
        let path = path();
        let backend = Arc::new(SqliteStorageBackend::connect(&path).await.unwrap());
        let (sink, worker) =
            SqliteResolveDetailWriter::channel(Arc::clone(&backend), 3, 2).unwrap();
        let writer = sink;
        for listener_id in ["listener-a", "listener-b", "listener-c"] {
            writer
                .try_record(ResolveEvent {
                    occurred_at: SystemTime::now(),
                    duration_millis: 8,
                    dns_core_duration_micros: 250,
                    request_digest: Arc::from("digest"),
                    listener_id: Arc::from(listener_id),
                    route_id: None,
                    client_ip: None,
                    client_bucket: None,
                    strategy_id: None,
                    upstream_id: None,
                    upstream_member_id: None,
                    upstream_used_id: None,
                    matched_rule_source: None,
                    matched_resource_id: None,
                    matched_rule_ordinal: None,
                    resource_version: None,
                    transport: TransportClass::Datagram,
                    qname: Arc::from("example.com."),
                    qtype: 1,
                    qclass: 1,
                    answers: Vec::new(),
                    rcode: 0,
                    cancellation_reason: None,
                    outcome: OutcomeClass::Success,
                    source: StatsSource::Upstream,
                    cache_status: CacheStatus::Miss,
                    runtime_revision: RuntimeRevision(1),
                })
                .unwrap();
        }
        let summary = worker.shutdown(deadline()).await.unwrap();
        assert_eq!(summary.committed, 3);
        assert_eq!(summary.evicted, 0);
        assert_eq!(summary.dropped, 0);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn sqlite_detail_worker_run_flushes_periodically_and_drains_on_cancel() {
        let path = path();
        let backend = Arc::new(SqliteStorageBackend::connect(&path).await.unwrap());
        let (sink, worker) =
            SqliteResolveDetailWriter::channel(Arc::clone(&backend), 2, 2).unwrap();
        let writer = sink;
        writer
            .try_record(ResolveEvent {
                occurred_at: SystemTime::now(),
                duration_millis: 8,
                dns_core_duration_micros: 250,
                request_digest: Arc::from("digest"),
                listener_id: Arc::from("listener"),
                route_id: None,
                client_ip: None,
                client_bucket: None,
                strategy_id: None,
                upstream_id: None,
                upstream_member_id: None,
                upstream_used_id: None,
                matched_rule_source: None,
                matched_resource_id: None,
                matched_rule_ordinal: None,
                resource_version: None,
                transport: TransportClass::Datagram,
                qname: Arc::from("example.com."),
                qtype: 1,
                qclass: 1,
                answers: Vec::new(),
                rcode: 0,
                cancellation_reason: None,
                outcome: OutcomeClass::Success,
                source: StatsSource::Upstream,
                cache_status: CacheStatus::Miss,
                runtime_revision: RuntimeRevision(1),
            })
            .unwrap();
        let cancellation = Cancellation::new();
        let task = tokio::spawn(worker.run(
            cancellation.clone(),
            Duration::from_millis(5),
            Duration::from_secs(5),
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancellation.cancel(CancelReason::Shutdown);
        let summary = task.await.unwrap().unwrap();
        assert_eq!(summary.flush.committed, 1);
        assert_eq!(summary.flush.evicted, 0);
        assert_eq!(summary.flush.dropped, 0);
        assert_eq!(summary.failed_flushes, 0);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn sqlite_detail_worker_flushes_a_full_batch_without_waiting_for_timer() {
        let path = path();
        let backend = Arc::new(SqliteStorageBackend::connect(&path).await.unwrap());
        let (writer, worker) =
            SqliteResolveDetailWriter::channel(Arc::clone(&backend), 2, 2).unwrap();
        let cancellation = Cancellation::new();
        let task = tokio::spawn(worker.run(
            cancellation.clone(),
            Duration::from_secs(60),
            Duration::from_secs(5),
        ));

        for listener_id in ["listener-a", "listener-b"] {
            writer
                .try_record(ResolveEvent {
                    occurred_at: SystemTime::now(),
                    duration_millis: 8,
                    dns_core_duration_micros: 250,
                    request_digest: Arc::from("digest"),
                    listener_id: Arc::from(listener_id),
                    route_id: None,
                    client_ip: None,
                    client_bucket: None,
                    strategy_id: None,
                    upstream_id: None,
                    upstream_member_id: None,
                    upstream_used_id: None,
                    matched_rule_source: None,
                    matched_resource_id: None,
                    matched_rule_ordinal: None,
                    resource_version: None,
                    transport: TransportClass::Datagram,
                    qname: Arc::from("example.com."),
                    qtype: 1,
                    qclass: 1,
                    answers: Vec::new(),
                    rcode: 0,
                    cancellation_reason: None,
                    outcome: OutcomeClass::Success,
                    source: StatsSource::Upstream,
                    cache_status: CacheStatus::Miss,
                    runtime_revision: RuntimeRevision(1),
                })
                .unwrap();
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
                    .fetch_one(&backend.pool)
                    .await
                    .unwrap();
                if count == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("full detail batch must commit before the 60 second timer");

        cancellation.cancel(CancelReason::Shutdown);
        let summary = task.await.unwrap().unwrap();
        assert_eq!(summary.flush.committed, 2);
        assert_eq!(summary.failed_flushes, 0);
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
