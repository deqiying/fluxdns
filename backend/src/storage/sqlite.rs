//! 业务 SQLite `StorageBackend` 首轮 adapter。

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, Sqlite, SqlitePool};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::dns::Deadline;
use crate::ports::storage::{
    SchemaVersion, StatsBatch, StorageBackend, StorageFlushSummary, StorageHealth,
    StorageOperation, StorageTransaction,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::STORAGE_SCHEMA_VERSION;
use super::resolve_log::{ResolveDetailRecord, ResolveDetailWriter};

const SQLITE_BUSY_TIMEOUT_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SqliteResolveDetailWriterBuildError {
    #[error("sqlite resolve detail queue capacity must be greater than zero")]
    ZeroCapacity,
    #[error("sqlite resolve detail batch size must be greater than zero")]
    ZeroBatchSize,
}

/// 将已脱敏的详情记录送入独立 SQLite writer channel。
pub struct SqliteResolveDetailWriter {
    sender: mpsc::Sender<ResolveDetailRecord>,
}

/// SQLite 详情 writer 的受管 flush 端；不在 DNS 请求线程执行数据库 I/O。
pub struct SqliteResolveDetailWorker {
    backend: Arc<SqliteStorageBackend>,
    receiver: mpsc::Receiver<ResolveDetailRecord>,
    pending: VecDeque<ResolveDetailRecord>,
    max_batch: usize,
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
            },
        ))
    }
}

impl ResolveDetailWriter for SqliteResolveDetailWriter {
    fn append(&mut self, record: &ResolveDetailRecord) -> Result<(), PortError> {
        self.sender
            .try_send(record.clone())
            .map_err(|error| match error {
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

    pub async fn flush(&mut self, deadline: Deadline) -> Result<u64, PortError> {
        while self.pending.len() < self.max_batch {
            match self.receiver.try_recv() {
                Ok(record) => self.pending.push_back(record),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
        if self.pending.is_empty() {
            return Ok(0);
        }
        let records = self
            .pending
            .iter()
            .take(self.max_batch)
            .cloned()
            .collect::<Vec<_>>();
        let committed = self.backend.write_detail_records(records, deadline).await?;
        for _ in 0..committed {
            let _ = self.pending.pop_front();
        }
        Ok(committed)
    }
}

#[derive(Clone)]
pub struct SqliteStorageBackend {
    pool: SqlitePool,
    path: Arc<PathBuf>,
    state: Arc<Mutex<SqliteStorageState>>,
    operation_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Copy)]
struct SqliteStorageState {
    health: StorageHealth,
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
        let path = path.into();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|_| SqliteStorageBackendBuildError::Directory)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(|_| SqliteStorageBackendBuildError::Connect)?;
        let has_meta = sqlx::query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'storage_meta' LIMIT 1",
        )
        .fetch_optional(&pool)
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Schema)?
        .is_some();
        if !has_meta {
            for statement in include_str!("../../migrations/0001_storage.sql").split(';') {
                let statement = statement.trim();
                if !statement.is_empty() {
                    sqlx::query(statement)
                        .execute(&pool)
                        .await
                        .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
                }
            }
        }
        sqlx::query(
            "INSERT OR IGNORE INTO storage_meta \
             (singleton, schema_version, database_id, created_at_utc, migrated_at_utc) \
             VALUES (1, ?, ?, ?, ?)",
        )
        .bind(i64::from(STORAGE_SCHEMA_VERSION.0))
        .bind(format!("fluxdns-{}", std::process::id()))
        .bind(unix_millis())
        .bind(unix_millis())
        .execute(&pool)
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
        Ok(Self {
            pool,
            path: Arc::new(path),
            state: Arc::new(Mutex::new(SqliteStorageState::default())),
            operation_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    fn available(&self, operation: &'static str) -> Result<(), PortError> {
        let state = self
            .state
            .lock()
            .map_err(|_| PortError::new(PortErrorClass::Internal, operation))?;
        match state.health {
            StorageHealth::Healthy => Ok(()),
            StorageHealth::Degraded | StorageHealth::Failed | StorageHealth::Stopping => {
                Err(PortError::new(PortErrorClass::Unavailable, operation))
            }
        }
    }

    fn mark_degraded(&self) {
        if let Ok(mut state) = self.state.lock()
            && state.health == StorageHealth::Healthy
        {
            state.health = StorageHealth::Degraded;
        }
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
        let _guard = self.operation_lock.lock().await;
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
        let _guard = self.operation_lock.lock().await;
        let mut sql_transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.execute"))?;
        for operation in transaction.operations {
            let result = match operation {
                StorageOperation::StatsBatch(batch) => {
                    apply_stats_batch(&mut sql_transaction, &batch).await
                }
                StorageOperation::ResolveBatch(batch) => {
                    apply_resolve_batch(&mut sql_transaction, &batch).await
                }
            };
            result?;
            check_deadline(deadline, "sqlite_storage.execute")?;
        }
        sql_transaction
            .commit()
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.execute"))
    }

    async fn write_detail_records(
        &self,
        records: Vec<ResolveDetailRecord>,
        deadline: Deadline,
    ) -> Result<u64, PortError> {
        check_deadline(deadline, "sqlite_storage.resolve_detail")?;
        self.available("sqlite_storage.resolve_detail")?;
        if records.is_empty() {
            return Ok(0);
        }
        let _guard = self.operation_lock.lock().await;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.resolve_detail"))?;
        apply_resolve_records(&mut transaction, &records).await?;
        check_deadline(deadline, "sqlite_storage.resolve_detail")?;
        transaction
            .commit()
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.resolve_detail"))?;
        Ok(records.len() as u64)
    }

    async fn checkpoint_now(&self, deadline: Deadline) -> Result<(), PortError> {
        check_deadline(deadline, "sqlite_storage.checkpoint")?;
        self.available("sqlite_storage.checkpoint")?;
        sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(&self.pool)
            .await
            .map_err(|error| self.database_error(error, "sqlite_storage.checkpoint"))?;
        Ok(())
    }

    fn database_error(&self, error: sqlx::Error, operation: &'static str) -> PortError {
        self.mark_degraded();
        match error {
            sqlx::Error::PoolClosed | sqlx::Error::Io(_) | sqlx::Error::Database(_) => {
                PortError::new(PortErrorClass::Unavailable, operation)
            }
            _ => PortError::new(PortErrorClass::Internal, operation),
        }
    }
}

impl std::fmt::Debug for SqliteStorageBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteStorageBackend")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl StorageBackend for SqliteStorageBackend {
    fn migrate(
        &self,
        target: SchemaVersion,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<SchemaVersion, PortError>> {
        Box::pin(self.migrate_now(target, deadline))
    }

    fn execute(
        &self,
        transaction: StorageTransaction,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(self.execute_now(transaction, deadline))
    }

    fn health_probe(&self, deadline: Deadline) -> PortFuture<'_, Result<StorageHealth, PortError>> {
        Box::pin(async move {
            check_deadline(deadline, "sqlite_storage.health_probe")?;
            let stopping = {
                let state = self.state.lock().map_err(|_| {
                    PortError::new(PortErrorClass::Internal, "sqlite_storage.health_probe")
                })?;
                state.health == StorageHealth::Stopping
            };
            if stopping {
                return Ok(StorageHealth::Stopping);
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
        })
    }

    fn checkpoint(&self, deadline: Deadline) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(self.checkpoint_now(deadline))
    }

    fn flush(&self, deadline: Deadline) -> PortFuture<'_, Result<StorageFlushSummary, PortError>> {
        Box::pin(async move {
            self.checkpoint_now(deadline).await?;
            Ok(StorageFlushSummary::default())
        })
    }

    fn shutdown(
        &self,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<StorageFlushSummary, PortError>> {
        Box::pin(async move {
            check_deadline(deadline, "sqlite_storage.shutdown")?;
            let _guard = self.operation_lock.lock().await;
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
        })
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
         (batch_id, max_event_seq, counter_epoch, committed_at_utc, payload_hash) \
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
    .bind(unix_millis())
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
    for record in records {
        let duration_millis = i64::try_from(record.duration_millis()).unwrap_or(i64::MAX);
        let request_digest = if record.has_request_digest() {
            "<present>"
        } else {
            "<absent>"
        };
        let route_id = record.has_route().then_some("<present>");
        let client_bucket = record.has_client_bucket().then_some("<present>");
        let strategy_id = record.has_strategy().then_some("<present>");
        let canonical_qname = format!("len:{}", record.qname_byte_len());
        sqlx::query(
            "INSERT INTO resolve_log \
             (event_time_utc, duration_millis, request_id_digest, listener_id, route_id, \
              client_bucket, strategy_id, canonical_qname, qtype, qclass, source, upstream_id, \
              rcode, cache_status, failure_class, cancellation_reason, runtime_revision, resource_revision) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(system_time_millis(record.occurred_at()))
        .bind(duration_millis)
        .bind(request_digest)
        .bind(record.listener_id())
        .bind(route_id)
        .bind(client_bucket)
        .bind(strategy_id)
        .bind(canonical_qname)
        .bind(i64::from(record.qtype()))
        .bind(i64::from(record.qclass()))
        .bind(Option::<&str>::None)
        .bind(Option::<&str>::None)
        .bind(0_i64)
        .bind(cache_status_name(record.cache_status()))
        .bind(Option::<&str>::None)
        .bind(Option::<&str>::None)
        .bind(i64::try_from(record.runtime_revision().0).unwrap_or(i64::MAX))
        .bind(Option::<&str>::None)
        .execute(&mut **transaction)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "sqlite_storage.resolve_batch"))?;
    }
    Ok(())
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

fn check_deadline(deadline: Deadline, operation: &'static str) -> Result<(), PortError> {
    if deadline.is_expired(Instant::now()) {
        Err(PortError::new(PortErrorClass::Timeout, operation))
    } else {
        Ok(())
    }
}

fn unix_millis() -> String {
    system_time_millis(SystemTime::now())
}

fn system_time_millis(time: SystemTime) -> String {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use super::{
        SqliteResolveDetailWriter, SqliteResolveDetailWriterBuildError, SqliteStorageBackend,
    };
    use crate::dns::{Deadline, RuntimeRevision, TransportClass};
    use crate::ports::storage::{
        ResolveEvent, ResolveEventSink, SchemaVersion, StatsBatch, StatsEvent, StorageBackend,
        StorageOperation, StorageTransaction,
    };
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::storage::ResolveLogWriter;
    use sqlx::Row;

    static NEXT_TEST_DB: AtomicU64 = AtomicU64::new(0);

    fn path() -> std::path::PathBuf {
        let id = NEXT_TEST_DB.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fluxdns-storage-{id}-{}.sqlite3",
            std::process::id()
        ))
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

    #[tokio::test]
    async fn migrates_commits_and_reopens_idempotent_stats_batch() {
        let path = path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        assert_eq!(
            backend.migrate(SchemaVersion(1), deadline()).await.unwrap(),
            SchemaVersion(1)
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
                occurred_at: SystemTime::now(),
                duration_started_at: Instant::now(),
                request_digest: Arc::from("digest"),
                listener_id: Arc::from("listener"),
                route_id: Some(Arc::from("route")),
                client_bucket: None,
                strategy_id: Some(Arc::from("strategy")),
                transport: TransportClass::Datagram,
                qname: Arc::from("example.com."),
                qtype: 1,
                qclass: 1,
                outcome: OutcomeClass::Success,
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
            "SELECT request_id_digest, route_id, client_bucket, strategy_id, canonical_qname \
             FROM resolve_log LIMIT 1",
        )
        .fetch_one(&backend.pool)
        .await
        .unwrap();
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
            Some("<present>")
        );
        assert_eq!(
            row.try_get::<String, _>("canonical_qname").unwrap(),
            "len:12"
        );
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
        let writer = ResolveLogWriter::new(true, 2, sink).unwrap();
        for listener_id in ["listener-a", "listener-b"] {
            writer
                .try_record(ResolveEvent {
                    occurred_at: SystemTime::now(),
                    duration_started_at: Instant::now(),
                    request_digest: Arc::from("digest"),
                    listener_id: Arc::from(listener_id),
                    route_id: None,
                    client_bucket: None,
                    strategy_id: None,
                    transport: TransportClass::Datagram,
                    qname: Arc::from("example.com."),
                    qtype: 1,
                    qclass: 1,
                    outcome: OutcomeClass::Success,
                    cache_status: CacheStatus::Miss,
                    runtime_revision: RuntimeRevision(1),
                })
                .unwrap();
        }
        assert_eq!(writer.flush().committed, 2);
        assert_eq!(worker.pending_len(), 2);
        assert_eq!(worker.flush(deadline()).await.unwrap(), 1);
        assert_eq!(worker.pending_len(), 1);
        assert_eq!(worker.flush(deadline()).await.unwrap(), 1);
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
        let writer = ResolveLogWriter::new(true, 1, sink).unwrap();
        writer
            .try_record(ResolveEvent {
                occurred_at: SystemTime::now(),
                duration_started_at: Instant::now(),
                request_digest: Arc::from("digest"),
                listener_id: Arc::from("listener"),
                route_id: None,
                client_bucket: None,
                strategy_id: None,
                transport: TransportClass::Datagram,
                qname: Arc::from("example.com."),
                qtype: 1,
                qclass: 1,
                outcome: OutcomeClass::Success,
                cache_status: CacheStatus::Miss,
                runtime_revision: RuntimeRevision(1),
            })
            .unwrap();
        assert_eq!(writer.flush().committed, 1);
        backend.shutdown(deadline()).await.unwrap();
        assert!(worker.flush(deadline()).await.is_err());
        assert_eq!(worker.pending_len(), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
