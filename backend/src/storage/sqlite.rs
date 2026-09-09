//! v2 统计 SQLite adapter，以及日分片复用的有界详情插入 helper。

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, Sqlite, SqlitePool};
use thiserror::Error;

use crate::dns::{CancelReason, Deadline};
use crate::ports::storage::{
    ResolveRuleSource, SchemaVersion, StatsBatch, StorageBackend, StorageFlushSummary,
    StorageHealth, StorageOperation, StorageTransaction,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::STORAGE_SCHEMA_VERSION;
use super::resolve_log::ResolveDetailRecord;
use super::retention::{
    RetentionAvailableRange, RetentionBootstrap, RetentionManifestEntry, RetentionManifestState,
    RetentionPlan, RetentionRunState, RetentionState, RetentionStatusMetadata,
};

const SQLITE_BUSY_TIMEOUT_MS: u64 = 2_000;
const V2_STORAGE_LAYOUT_VERSION: i64 = 1;
const V2_STORAGE_LAYOUT_KIND: &str = "statistics-v2";

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
pub(super) enum InjectedSqliteFault {
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
    #[error("existing sqlite storage uses the legacy layout; use a new development directory")]
    LegacyLayout,
    #[error("sqlite storage layout marker is invalid")]
    InvalidLayout,
}

impl SqliteStorageBackend {
    /// 打开当前 v2 统计布局，拒绝未标记或版本不符的数据库。
    pub async fn connect(path: impl Into<PathBuf>) -> Result<Self, SqliteStorageBackendBuildError> {
        Self::connect_with_deadline(
            path,
            Deadline::new(Instant::now() + super::service::DEFAULT_STORAGE_OPERATION_TIMEOUT),
        )
        .await
    }

    /// 连接、原子初始化和版本校验共享启动预算；成功前不创建任何 writer。
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
        let initialized = initialize_or_validate_v2_layout(&mut initialization).await?;
        if initialized {
            for statement in include_str!("../../migrations/0001_statistics.sql").split(';') {
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
                 (singleton, schema_version, database_id, created_at_utc_millis, migrated_at_utc_millis) \
                 VALUES (1, ?, ?, ?, ?)",
            )
            .bind(i64::from(STORAGE_SCHEMA_VERSION.0))
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
        let version: i64 =
            sqlx::query_scalar("SELECT schema_version FROM storage_meta WHERE singleton = 1")
                .fetch_one(&mut *initialization)
                .await
                .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
        if version != i64::from(STORAGE_SCHEMA_VERSION.0) {
            return Err(SqliteStorageBackendBuildError::Schema);
        }
        initialization
            .commit()
            .await
            .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
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

    /// 恢复共同水位，并从持久化 replay 下界/ledger 高水位选择新的 batch ID 起点。
    pub(crate) async fn retention_bootstrap(
        &self,
        deadline: Deadline,
    ) -> Result<RetentionBootstrap, PortError> {
        let operation = "sqlite_storage.retention_bootstrap";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            let state = sqlx::query(
                "SELECT revision, watermark_revision, retired_before_day_utc, reference_day_utc, \
                 target_days, sampled_detail_bytes, reference_size_bytes, replay_floor_batch_id, \
                 published_at_utc_millis FROM retention_state WHERE singleton = 1",
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?
            .map(|row| retention_state_from_row(&row, operation))
            .transpose()?;
            let max_batch_id: Option<i64> =
                sqlx::query_scalar("SELECT MAX(batch_id) FROM stats_batch_ledger")
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|error| self.database_error(error, operation))?;
            let ledger_next = match max_batch_id {
                Some(value) if value > 0 => u64::try_from(value)
                    .ok()
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(|| PortError::new(PortErrorClass::ResourceExhausted, operation))?,
                Some(_) => return Err(PortError::new(PortErrorClass::CorruptData, operation)),
                None => 1,
            };
            let next_stats_batch_id = state.as_ref().map_or(ledger_next, |state| {
                ledger_next.max(state.replay_floor_batch_id)
            });
            if next_stats_batch_id == 0 || i64::try_from(next_stats_batch_id).is_err() {
                return Err(PortError::new(PortErrorClass::ResourceExhausted, operation));
            }
            Ok(RetentionBootstrap {
                state,
                next_stats_batch_id,
            })
        })
        .await
    }

    /// 新空库以启动当日本地日期为调度基线；已有水位的升级库仍保留补跑资格。
    pub(crate) async fn initialize_retention_schedule(
        &self,
        local_day: i32,
        local_second: u32,
        deadline: Deadline,
    ) -> Result<(), PortError> {
        let operation = "sqlite_storage.retention_schedule_init";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            validate_retention_day(local_day, operation)?;
            if local_second >= 24 * 60 * 60 {
                return Err(PortError::new(PortErrorClass::InvalidInput, operation));
            }
            let baseline_day = if local_second < super::retention::RETENTION_SCHEDULE_LOCAL_SECOND {
                local_day
                    .checked_sub(1)
                    .ok_or_else(|| PortError::new(PortErrorClass::InvalidInput, operation))?
            } else {
                local_day
            };
            validate_retention_day(baseline_day, operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            sqlx::query(
                "UPDATE retention_run_state SET last_success_local_day = ? \
                 WHERE singleton = 1 AND last_success_local_day IS NULL \
                 AND NOT EXISTS (SELECT 1 FROM retention_state WHERE singleton = 1)",
            )
            .bind(i64::from(baseline_day))
            .execute(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn retention_run_state(
        &self,
        deadline: Deadline,
    ) -> Result<RetentionRunState, PortError> {
        let operation = "sqlite_storage.retention_run_state";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            let row = sqlx::query(
                "SELECT last_attempt_local_day, last_success_local_day, \
                 last_attempted_at_utc_millis, last_succeeded_at_utc_millis, \
                 consecutive_failures, last_error_code \
                 FROM retention_run_state WHERE singleton = 1",
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            retention_run_state_from_row(&row, operation)
        })
        .await
    }

    pub(crate) async fn begin_retention_run(
        &self,
        local_day: i32,
        attempted_at: SystemTime,
        deadline: Deadline,
    ) -> Result<(), PortError> {
        let operation = "sqlite_storage.retention_run_begin";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            validate_retention_day(local_day, operation)?;
            let attempted_at = system_time_utc_millis(attempted_at, operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            let changed = sqlx::query(
                "UPDATE retention_run_state SET last_attempt_local_day = ?, \
                 last_attempted_at_utc_millis = ?, last_error_code = NULL \
                 WHERE singleton = 1",
            )
            .bind(i64::from(local_day))
            .bind(attempted_at)
            .execute(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            if changed.rows_affected() != 1 {
                return Err(PortError::new(PortErrorClass::CorruptData, operation));
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn finish_retention_run(
        &self,
        local_day: i32,
        finished_at: SystemTime,
        error_code: Option<&'static str>,
        deadline: Deadline,
    ) -> Result<(), PortError> {
        let operation = "sqlite_storage.retention_run_finish";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            validate_retention_day(local_day, operation)?;
            validate_retention_error_code(error_code, operation)?;
            let finished_at = system_time_utc_millis(finished_at, operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            let changed = if let Some(error_code) = error_code {
                sqlx::query(
                    "UPDATE retention_run_state SET last_attempt_local_day = ?, \
                     last_attempted_at_utc_millis = ?, \
                     consecutive_failures = consecutive_failures + 1, last_error_code = ? \
                     WHERE singleton = 1 AND consecutive_failures < 4294967295",
                )
                .bind(i64::from(local_day))
                .bind(finished_at)
                .bind(error_code)
                .execute(&self.pool)
                .await
            } else {
                sqlx::query(
                    "UPDATE retention_run_state SET last_attempt_local_day = ?, \
                     last_success_local_day = CASE \
                         WHEN last_success_local_day IS NULL OR last_success_local_day < ? \
                         THEN ? ELSE last_success_local_day END, \
                     last_attempted_at_utc_millis = ?, last_succeeded_at_utc_millis = ?, \
                     consecutive_failures = 0, last_error_code = NULL WHERE singleton = 1",
                )
                .bind(i64::from(local_day))
                .bind(i64::from(local_day))
                .bind(i64::from(local_day))
                .bind(finished_at)
                .bind(finished_at)
                .execute(&self.pool)
                .await
            }
            .map_err(|error| self.database_error(error, operation))?;
            if changed.rows_affected() != 1 {
                return Err(PortError::new(PortErrorClass::ResourceExhausted, operation));
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn pending_retention_reclaims(
        &self,
        deadline: Deadline,
    ) -> Result<Vec<RetentionManifestEntry>, PortError> {
        let operation = "sqlite_storage.retention_manifest";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            let rows = sqlx::query(
                "SELECT day_utc, retired_revision, state, attempts, last_error_code, \
                 updated_at_utc_millis FROM retention_detail_manifest \
                 WHERE state IN ('pending', 'failed') ORDER BY day_utc ASC",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            rows.iter()
                .map(|row| retention_manifest_from_row(row, operation))
                .collect()
        })
        .await
    }

    pub(crate) async fn pending_retention_reclaim_count(
        &self,
        deadline: Deadline,
    ) -> Result<u32, PortError> {
        let operation = "sqlite_storage.retention_manifest_count";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM retention_detail_manifest \
                 WHERE state IN ('pending', 'failed')",
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            u32::try_from(count).map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))
        })
        .await
    }

    pub(crate) async fn finish_retention_reclaim(
        &self,
        day_utc: i32,
        error_code: Option<&'static str>,
        deadline: Deadline,
    ) -> Result<(), PortError> {
        let operation = "sqlite_storage.retention_manifest_finish";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            validate_retention_day(day_utc, operation)?;
            validate_retention_error_code(error_code, operation)?;
            let updated_at = system_time_utc_millis(SystemTime::now(), operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            let changed = sqlx::query(
                "UPDATE retention_detail_manifest SET state = ?, attempts = attempts + 1, \
                 last_error_code = ?, updated_at_utc_millis = ? \
                 WHERE day_utc = ? AND state IN ('pending', 'failed') \
                 AND attempts < 4294967295",
            )
            .bind(if error_code.is_some() {
                RetentionManifestState::Failed.as_str()
            } else {
                RetentionManifestState::Reclaimed.as_str()
            })
            .bind(error_code)
            .bind(updated_at)
            .bind(i64::from(day_utc))
            .execute(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            if changed.rows_affected() != 1 {
                return Err(PortError::new(PortErrorClass::CorruptData, operation));
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn retention_status_metadata(
        &self,
        deadline: Deadline,
    ) -> Result<RetentionStatusMetadata, PortError> {
        let operation = "sqlite_storage.retention_status";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            let _guard = self.lock_operation(deadline, operation).await?;
            let published = sqlx::query(
                "SELECT revision, watermark_revision, retired_before_day_utc, reference_day_utc, \
                 target_days, sampled_detail_bytes, reference_size_bytes, replay_floor_batch_id, \
                 published_at_utc_millis FROM retention_state WHERE singleton = 1",
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?
            .map(|row| retention_state_from_row(&row, operation))
            .transpose()?;
            let (stats_from, stats_to): (Option<i64>, Option<i64>) =
                sqlx::query_as("SELECT MIN(day_utc), MAX(day_utc) FROM stats_daily_total")
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|error| self.database_error(error, operation))?;
            let stats_available = RetentionAvailableRange {
                from_day_utc: optional_retention_day(stats_from, operation)?,
                to_day_utc: optional_retention_day(stats_to, operation)?,
            };
            let (pending, failed): (i64, i64) = sqlx::query_as(
                "SELECT COUNT(*) FILTER (WHERE state = 'pending'), \
                 COUNT(*) FILTER (WHERE state = 'failed') FROM retention_detail_manifest",
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            let last_reclaim_at: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(updated_at_utc_millis) FROM retention_detail_manifest \
                 WHERE state = 'reclaimed'",
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            let run_row = sqlx::query(
                "SELECT last_attempt_local_day, last_success_local_day, \
                 last_attempted_at_utc_millis, last_succeeded_at_utc_millis, \
                 consecutive_failures, last_error_code \
                 FROM retention_run_state WHERE singleton = 1",
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            Ok(RetentionStatusMetadata {
                published,
                stats_available,
                pending_reclaims: u32::try_from(pending)
                    .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?,
                failed_reclaims: u32::try_from(failed)
                    .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?,
                last_reclaim_at: optional_system_time_from_millis(last_reclaim_at, operation)?,
                run: retention_run_state_from_row(&run_row, operation)?,
            })
        })
        .await
    }

    /// 在统计事务中发布单调水位、清理旧统计并登记待回收详情日。
    pub(crate) async fn publish_retention(
        &self,
        plan: RetentionPlan,
        manifest_days: &[i32],
        replay_floor_batch_id: u64,
        deadline: Deadline,
    ) -> Result<RetentionState, PortError> {
        let operation = "sqlite_storage.publish_retention";
        run_with_deadline(deadline, operation, async {
            self.available(operation)?;
            if replay_floor_batch_id == 0
                || i64::try_from(replay_floor_batch_id).is_err()
                || !plan.is_valid()
            {
                return Err(PortError::new(PortErrorClass::InvalidInput, operation));
            }
            let _guard = self.lock_operation(deadline, operation).await?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|error| self.database_error(error, operation))?;
            let current = sqlx::query(
                "SELECT revision, watermark_revision, retired_before_day_utc, reference_day_utc, \
                 target_days, sampled_detail_bytes, reference_size_bytes, replay_floor_batch_id, \
                 published_at_utc_millis FROM retention_state WHERE singleton = 1",
            )
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|error| self.database_error(error, operation))?
            .map(|row| retention_state_from_row(&row, operation))
            .transpose()?;
            let revision = current
                .as_ref()
                .map_or(Ok(1), |state| state.revision.checked_add(1).ok_or(()))
                .map_err(|()| PortError::new(PortErrorClass::ResourceExhausted, operation))?;
            let retired_before_day_utc = current.as_ref().map_or(
                plan.keep_from_day_utc,
                |state| state.retired_before_day_utc.max(plan.keep_from_day_utc),
            );
            let watermark_revision = match current.as_ref() {
                None => 1,
                Some(state) if retired_before_day_utc > state.retired_before_day_utc => state
                    .watermark_revision
                    .checked_add(1)
                    .ok_or_else(|| {
                        PortError::new(PortErrorClass::ResourceExhausted, operation)
                    })?,
                Some(state) => state.watermark_revision,
            };
            if i64::try_from(revision).is_err() || i64::try_from(watermark_revision).is_err() {
                return Err(PortError::new(
                    PortErrorClass::ResourceExhausted,
                    operation,
                ));
            }
            let replay_floor_batch_id = current.as_ref().map_or(
                replay_floor_batch_id,
                |state| state.replay_floor_batch_id.max(replay_floor_batch_id),
            );
            let published_at = SystemTime::now();
            let published_at_millis = system_time_utc_millis(published_at, operation)?;

            sqlx::query("DELETE FROM stats_daily_dimension WHERE day_utc < ?")
                .bind(i64::from(retired_before_day_utc))
                .execute(&mut *transaction)
                .await
                .map_err(|error| self.database_error(error, operation))?;
            sqlx::query("DELETE FROM stats_daily_total WHERE day_utc < ?")
                .bind(i64::from(retired_before_day_utc))
                .execute(&mut *transaction)
                .await
                .map_err(|error| self.database_error(error, operation))?;
            sqlx::query("DELETE FROM stats_batch_ledger WHERE batch_id < ?")
                .bind(i64::try_from(replay_floor_batch_id).unwrap())
                .execute(&mut *transaction)
                .await
                .map_err(|error| self.database_error(error, operation))?;

            let mut unique_days = manifest_days.to_vec();
            unique_days.sort_unstable();
            unique_days.dedup();
            for day_utc in unique_days
                .into_iter()
                .filter(|day_utc| *day_utc < retired_before_day_utc)
            {
                if super::detail_shards::format_shard_file_name(day_utc).is_none() {
                    return Err(PortError::new(PortErrorClass::InvalidInput, operation));
                }
                sqlx::query(
                    "INSERT INTO retention_detail_manifest \
                     (day_utc, retired_revision, state, attempts, last_error_code, updated_at_utc_millis) \
                     VALUES (?, ?, 'pending', 0, NULL, ?) \
                     ON CONFLICT(day_utc) DO UPDATE SET \
                         retired_revision = excluded.retired_revision, \
                         state = CASE WHEN retention_detail_manifest.state = 'reclaimed' \
                             THEN 'pending' ELSE retention_detail_manifest.state END, \
                         attempts = CASE WHEN retention_detail_manifest.state = 'reclaimed' \
                             THEN 0 ELSE retention_detail_manifest.attempts END, \
                         last_error_code = CASE WHEN retention_detail_manifest.state = 'reclaimed' \
                             THEN NULL ELSE retention_detail_manifest.last_error_code END, \
                         updated_at_utc_millis = excluded.updated_at_utc_millis",
                )
                .bind(i64::from(day_utc))
                .bind(i64::try_from(watermark_revision).unwrap())
                .bind(published_at_millis)
                .execute(&mut *transaction)
                .await
                .map_err(|error| self.database_error(error, operation))?;
            }

            sqlx::query(
                "INSERT INTO retention_state \
                 (singleton, revision, watermark_revision, retired_before_day_utc, reference_day_utc, \
                  target_days, sampled_detail_bytes, reference_size_bytes, replay_floor_batch_id, \
                  published_at_utc_millis) \
                 VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(singleton) DO UPDATE SET \
                     revision = excluded.revision, \
                     watermark_revision = excluded.watermark_revision, \
                     retired_before_day_utc = excluded.retired_before_day_utc, \
                     reference_day_utc = excluded.reference_day_utc, \
                     target_days = excluded.target_days, \
                     sampled_detail_bytes = excluded.sampled_detail_bytes, \
                     reference_size_bytes = excluded.reference_size_bytes, \
                     replay_floor_batch_id = excluded.replay_floor_batch_id, \
                     published_at_utc_millis = excluded.published_at_utc_millis",
            )
            .bind(i64::try_from(revision).unwrap())
            .bind(i64::try_from(watermark_revision).unwrap())
            .bind(i64::from(retired_before_day_utc))
            .bind(i64::from(plan.reference_day_utc))
            .bind(i64::from(plan.target_days))
            .bind(i64::try_from(plan.sampled_detail_bytes).map_err(|_| {
                PortError::new(PortErrorClass::ResourceExhausted, operation)
            })?)
            .bind(i64::try_from(plan.policy.reference_size_bytes).map_err(|_| {
                PortError::new(PortErrorClass::ResourceExhausted, operation)
            })?)
            .bind(i64::try_from(replay_floor_batch_id).unwrap())
            .bind(published_at_millis)
            .execute(&mut *transaction)
            .await
            .map_err(|error| self.database_error(error, operation))?;
            check_deadline(deadline, operation)?;
            transaction
                .commit()
                .await
                .map_err(|error| self.database_error(error, operation))?;
            self.mark_healthy();
            Ok(RetentionState {
                revision,
                watermark_revision,
                retired_before_day_utc,
                reference_day_utc: plan.reference_day_utc,
                target_days: plan.target_days,
                sampled_detail_bytes: plan.sampled_detail_bytes,
                reference_size_bytes: plan.policy.reference_size_bytes,
                replay_floor_batch_id,
                published_at,
            })
        })
        .await
    }

    #[cfg(test)]
    pub(super) fn inject_fault(&self, fault: InjectedSqliteFault) {
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

async fn initialize_or_validate_v2_layout(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
) -> Result<bool, SqliteStorageBackendBuildError> {
    let has_layout = sqlx::query(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'fluxdns_layout' LIMIT 1",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| SqliteStorageBackendBuildError::Schema)?
    .is_some();
    if has_layout {
        let row =
            sqlx::query("SELECT kind, layout_version FROM fluxdns_layout WHERE singleton = 1")
                .fetch_optional(&mut **transaction)
                .await
                .map_err(|_| SqliteStorageBackendBuildError::InvalidLayout)?
                .ok_or(SqliteStorageBackendBuildError::InvalidLayout)?;
        let kind = row
            .try_get::<String, _>("kind")
            .map_err(|_| SqliteStorageBackendBuildError::InvalidLayout)?;
        let version = row
            .try_get::<i64, _>("layout_version")
            .map_err(|_| SqliteStorageBackendBuildError::InvalidLayout)?;
        if kind != V2_STORAGE_LAYOUT_KIND || version != V2_STORAGE_LAYOUT_VERSION {
            return Err(SqliteStorageBackendBuildError::InvalidLayout);
        }
        return Ok(false);
    }

    let existing_table = sqlx::query(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' LIMIT 1",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| SqliteStorageBackendBuildError::Schema)?
    .is_some();
    if existing_table {
        return Err(SqliteStorageBackendBuildError::LegacyLayout);
    }
    sqlx::query(
        "CREATE TABLE fluxdns_layout (\
         singleton INTEGER PRIMARY KEY CHECK (singleton = 1), \
         kind TEXT NOT NULL, layout_version INTEGER NOT NULL)",
    )
    .execute(&mut **transaction)
    .await
    .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
    sqlx::query("INSERT INTO fluxdns_layout (singleton, kind, layout_version) VALUES (1, ?, ?)")
        .bind(V2_STORAGE_LAYOUT_KIND)
        .bind(V2_STORAGE_LAYOUT_VERSION)
        .execute(&mut **transaction)
        .await
        .map_err(|_| SqliteStorageBackendBuildError::Schema)?;
    Ok(true)
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
    let retired_before_day_utc: Option<i64> = sqlx::query_scalar(
        "SELECT retired_before_day_utc FROM retention_state WHERE singleton = 1",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| PortError::new(PortErrorClass::Unavailable, "sqlite_storage.stats_batch"))?;
    for event in batch.events.iter().filter(|event| {
        retired_before_day_utc.is_none_or(|watermark| i64::from(event.day_utc()) >= watermark)
    }) {
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

pub(super) async fn apply_resolve_records(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    records: &[ResolveDetailRecord],
) -> Result<Vec<i64>, PortError> {
    let mut row_ids = Vec::with_capacity(records.len());
    for record in records {
        let duration_millis = i64::try_from(record.duration_millis()).unwrap_or(i64::MAX);
        let dns_core_duration_micros =
            i64::try_from(record.dns_core_duration_micros()).unwrap_or(i64::MAX);
        let request_digest = if record.has_request_digest() {
            "<present>"
        } else {
            "<absent>"
        };
        let route_id = record.has_route().then_some("<present>");
        let client_id = record.client_id();
        let client_ip = record.client_ip().map(|value| value.to_string());
        let client_match_source = record.client_match_source().map(client_match_source_name);
        let matched_client_id = record.matched_client_id();
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
        let result = sqlx::query(
            "INSERT INTO resolve_log \
             (event_time_utc_millis, duration_millis, dns_core_duration_micros, request_id_digest, listener_id, route_id, \
               client_bucket, strategy_id, canonical_qname, qtype, qclass, source, upstream_id, \
               upstream_member_id, matched_rule_source, matched_resource_id, matched_rule_ordinal, \
               rcode, cache_status, failure_class, cancellation_reason, runtime_revision, resource_revision, \
               transport, client_ip, upstream_used_id, answer_count, answers_truncated, answer_summary_json, \
               client_id, client_match_source, matched_client_id) \
              VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(client_id)
        .bind(client_match_source)
        .bind(matched_client_id)
        .execute(&mut **transaction)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "sqlite_storage.resolve_batch"))?;
        row_ids.push(result.last_insert_rowid());
    }
    Ok(row_ids)
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

fn client_match_source_name(value: crate::ports::observation::ClientMatchSource) -> &'static str {
    match value {
        crate::ports::observation::ClientMatchSource::Id => "id",
        crate::ports::observation::ClientMatchSource::Ip => "ip",
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

fn retention_state_from_row(
    row: &sqlx::sqlite::SqliteRow,
    operation: &'static str,
) -> Result<RetentionState, PortError> {
    let positive = |column: &str| {
        row.try_get::<i64, _>(column)
            .ok()
            .filter(|value| *value > 0)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))
    };
    let revision = positive("revision")?;
    let watermark_revision = positive("watermark_revision")?;
    let retired_before_day_utc = row
        .try_get::<i64, _>("retired_before_day_utc")
        .ok()
        .and_then(|value| i32::try_from(value).ok())
        .filter(|day| super::detail_shards::format_shard_file_name(*day).is_some())
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    let reference_day_utc = row
        .try_get::<i64, _>("reference_day_utc")
        .ok()
        .and_then(|value| i32::try_from(value).ok())
        .filter(|day| super::detail_shards::format_shard_file_name(*day).is_some())
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    let target_days = u32::try_from(positive("target_days")?)
        .ok()
        .filter(|value| *value <= super::retention::MAX_RETENTION_DAYS)
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    let sampled_detail_bytes = u64::try_from(
        row.try_get::<i64, _>("sampled_detail_bytes")
            .ok()
            .filter(|value| *value >= 0)
            .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?,
    )
    .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
    let reference_size_bytes = positive("reference_size_bytes")?;
    if reference_size_bytes > super::retention::MAX_RETENTION_REFERENCE_SIZE_BYTES {
        return Err(PortError::new(PortErrorClass::CorruptData, operation));
    }
    let replay_floor_batch_id = positive("replay_floor_batch_id")?;
    let published_at_millis = u64::try_from(
        row.try_get::<i64, _>("published_at_utc_millis")
            .ok()
            .filter(|value| *value >= 0)
            .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?,
    )
    .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
    let published_at = UNIX_EPOCH
        .checked_add(Duration::from_millis(published_at_millis))
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    Ok(RetentionState {
        revision,
        watermark_revision,
        retired_before_day_utc,
        reference_day_utc,
        target_days,
        sampled_detail_bytes,
        reference_size_bytes,
        replay_floor_batch_id,
        published_at,
    })
}

fn retention_run_state_from_row(
    row: &sqlx::sqlite::SqliteRow,
    operation: &'static str,
) -> Result<RetentionRunState, PortError> {
    let last_error_code = row
        .try_get::<Option<String>, _>("last_error_code")
        .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
    if !retention_error_code_is_valid(last_error_code.as_deref()) {
        return Err(PortError::new(PortErrorClass::CorruptData, operation));
    }
    let consecutive_failures = row
        .try_get::<i64, _>("consecutive_failures")
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    Ok(RetentionRunState {
        last_attempt_local_day: optional_retention_day(
            row.try_get::<Option<i64>, _>("last_attempt_local_day")
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?,
            operation,
        )?,
        last_success_local_day: optional_retention_day(
            row.try_get::<Option<i64>, _>("last_success_local_day")
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?,
            operation,
        )?,
        last_attempted_at: optional_system_time_from_millis(
            row.try_get::<Option<i64>, _>("last_attempted_at_utc_millis")
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?,
            operation,
        )?,
        last_succeeded_at: optional_system_time_from_millis(
            row.try_get::<Option<i64>, _>("last_succeeded_at_utc_millis")
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?,
            operation,
        )?,
        consecutive_failures,
        last_error_code,
    })
}

fn retention_manifest_from_row(
    row: &sqlx::sqlite::SqliteRow,
    operation: &'static str,
) -> Result<RetentionManifestEntry, PortError> {
    let day_utc = row
        .try_get::<i64, _>("day_utc")
        .ok()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    validate_retention_day(day_utc, operation)?;
    let retired_revision = row
        .try_get::<i64, _>("retired_revision")
        .ok()
        .filter(|value| *value > 0)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    let state = row
        .try_get::<String, _>("state")
        .ok()
        .and_then(|value| RetentionManifestState::parse(&value))
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    let attempts = row
        .try_get::<i64, _>("attempts")
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    let last_error_code = row
        .try_get::<Option<String>, _>("last_error_code")
        .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
    if !retention_error_code_is_valid(last_error_code.as_deref()) {
        return Err(PortError::new(PortErrorClass::CorruptData, operation));
    }
    let updated_at = optional_system_time_from_millis(
        Some(
            row.try_get::<i64, _>("updated_at_utc_millis")
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?,
        ),
        operation,
    )?
    .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))?;
    Ok(RetentionManifestEntry {
        day_utc,
        retired_revision,
        state,
        attempts,
        last_error_code,
        updated_at,
    })
}

fn validate_retention_day(day: i32, operation: &'static str) -> Result<(), PortError> {
    if super::detail_shards::format_shard_file_name(day).is_some() {
        Ok(())
    } else {
        Err(PortError::new(PortErrorClass::InvalidInput, operation))
    }
}

fn optional_retention_day(
    day: Option<i64>,
    operation: &'static str,
) -> Result<Option<i32>, PortError> {
    day.map(|value| {
        let value = i32::try_from(value)
            .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
        validate_retention_day(value, operation)
            .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
        Ok(value)
    })
    .transpose()
}

fn validate_retention_error_code(
    code: Option<&str>,
    operation: &'static str,
) -> Result<(), PortError> {
    if retention_error_code_is_valid(code) {
        Ok(())
    } else {
        Err(PortError::new(PortErrorClass::InvalidInput, operation))
    }
}

fn retention_error_code_is_valid(code: Option<&str>) -> bool {
    code.is_none_or(|code| {
        !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    })
}

fn optional_system_time_from_millis(
    millis: Option<i64>,
    operation: &'static str,
) -> Result<Option<SystemTime>, PortError> {
    millis
        .map(|millis| {
            let millis = u64::try_from(millis)
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
            UNIX_EPOCH
                .checked_add(Duration::from_millis(millis))
                .ok_or_else(|| PortError::new(PortErrorClass::CorruptData, operation))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use super::{
        InjectedSqliteFault, SqliteConnectOptions, SqlitePoolOptions, SqliteStorageBackend,
    };
    use crate::dns::Deadline;
    use crate::ports::storage::{
        StatsBatch, StatsEvent, StorageBackend, StorageOperation, StorageTransaction,
    };
    use sqlx::Row;

    static NEXT_TEST_DB: AtomicU64 = AtomicU64::new(0);

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
    async fn v2_layout_initializes_reopens_and_rejects_legacy_database() {
        let fresh = path();
        let opened = SqliteStorageBackend::connect_with_deadline(
            &fresh,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .unwrap();
        let marker: (String, i64) =
            sqlx::query_as("SELECT kind, layout_version FROM fluxdns_layout WHERE singleton = 1")
                .fetch_one(&opened.pool)
                .await
                .unwrap();
        assert_eq!(marker, ("statistics-v2".to_owned(), 1));
        let details: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'resolve_log'")
                .fetch_one(&opened.pool)
                .await
                .unwrap();
        assert_eq!(details, 0, "统计库不得创建旧单库详情表");
        drop(opened);
        SqliteStorageBackend::connect_with_deadline(
            &fresh,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .unwrap();

        let legacy = path();
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&legacy)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE old_detail (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert!(matches!(
            SqliteStorageBackend::connect_with_deadline(
                &legacy,
                Deadline::new(Instant::now() + Duration::from_secs(5))
            )
            .await,
            Err(super::SqliteStorageBackendBuildError::LegacyLayout)
        ));
        let _ = std::fs::remove_file(fresh);
        let _ = std::fs::remove_file(legacy);
    }

    #[tokio::test]
    async fn unmarked_partial_layout_is_rejected_without_initialization() {
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
            Err(super::SqliteStorageBackendBuildError::LegacyLayout)
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

    #[tokio::test]
    async fn contract_v4_newer_schema_is_rejected_without_mutation() {
        let backend = SqliteStorageBackend::connect(path()).await.unwrap();
        let newer = i64::from(crate::storage::STORAGE_SCHEMA_VERSION.0) + 1;
        sqlx::query("UPDATE storage_meta SET schema_version=?")
            .bind(newer)
            .execute(&backend.pool)
            .await
            .unwrap();
        let before: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name",
        )
        .fetch_all(&backend.pool)
        .await
        .unwrap();
        assert!(matches!(
            SqliteStorageBackend::connect(backend.path.as_ref()).await,
            Err(super::SqliteStorageBackendBuildError::Schema)
        ));
        let after: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name",
        )
        .fetch_all(&backend.pool)
        .await
        .unwrap();
        assert_eq!(before, after);
        let version: i64 = sqlx::query_scalar("SELECT schema_version FROM storage_meta")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
        assert_eq!(version, newer);
        backend.shutdown(deadline()).await.unwrap();
    }

    // V4-S01 / V9-S-local：三轮真实 SQLite 事务失败/恢复，UTC 午夜、late event 与重试去重。
    #[tokio::test]
    async fn contract_v4_midnight_late_events_and_repeated_sqlite_recovery() {
        use crate::storage::{PersistenceGapState, StatsPersistenceWorker, day_utc};
        let backend = Arc::new(SqliteStorageBackend::connect(path()).await.unwrap());
        let worker = StatsPersistenceWorker::new(backend.clone());
        let midnight = std::time::UNIX_EPOCH + Duration::from_secs(20_001 * 86_400);
        let previous_day = day_utc(midnight - Duration::from_millis(1)).unwrap();
        let current_day = day_utc(midnight).unwrap();
        assert_eq!((previous_day, current_day), (20_000, 20_001));
        for cycle in 0..3 {
            worker.record_request(current_day, vec![]).unwrap();
            worker.record_request(previous_day, vec![]).unwrap();
            sqlx::query(
                "CREATE TRIGGER reject_contract_stats BEFORE INSERT ON stats_daily_total \
                 BEGIN SELECT RAISE(ABORT, 'contract stats failure'); END",
            )
            .execute(&backend.pool)
            .await
            .unwrap();
            assert!(worker.flush(deadline()).await.is_err());
            assert_eq!(worker.pending_batch_count(), 1);
            let ledger_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stats_batch_ledger")
                .fetch_one(&backend.pool)
                .await
                .unwrap();
            assert_eq!(ledger_count, cycle * 2);
            // 新 epoch 收到前一天的晚到事件；不能用 flush 当天覆盖事件日。
            worker.record_request(previous_day, vec![]).unwrap();
            assert!(matches!(
                worker.persistence_gap(),
                PersistenceGapState::ActiveAndPending {
                    batch_count: 1,
                    active_event_count: 1,
                    pending_event_count: 2,
                    ..
                }
            ));
            sqlx::query("DROP TRIGGER reject_contract_stats")
                .execute(&backend.pool)
                .await
                .unwrap();
            let summary = worker.flush(deadline()).await.unwrap();
            assert_eq!(summary.events_committed, 3);
            assert_eq!(summary.batches_committed, 2);
            assert_eq!(worker.pending_batch_count(), 0);
            assert!(matches!(
                worker.persistence_gap(),
                PersistenceGapState::Clear
            ));
            worker.flush(deadline()).await.unwrap();
            let totals: Vec<(i64, i64)> =
                sqlx::query_as("SELECT * FROM stats_daily_total ORDER BY day_utc")
                    .fetch_all(&backend.pool)
                    .await
                    .unwrap();
            assert_eq!(totals, vec![(20_000, (cycle + 1) * 2), (20_001, cycle + 1)]);
        }
        let ledger: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT batch_id, max_event_seq FROM stats_batch_ledger ORDER BY batch_id",
        )
        .fetch_all(&backend.pool)
        .await
        .unwrap();
        assert_eq!(ledger.len(), 6);
        assert_eq!(ledger.last().unwrap().1, 9);
        assert!(
            ledger
                .windows(2)
                .all(|rows| rows[0].0 < rows[1].0 && rows[0].1 < rows[1].1)
        );
        let path = backend.path.as_ref().clone();
        backend.shutdown(deadline()).await.unwrap();
        drop(worker);
        drop(backend);
        let reopened = SqliteStorageBackend::connect(path).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT SUM(total_requests) FROM stats_daily_total")
            .fetch_one(&reopened.pool)
            .await
            .unwrap();
        assert_eq!(count, 9);
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&reopened.pool)
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
        reopened.shutdown(deadline()).await.unwrap();
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
    async fn integer_metadata_timestamp_constraints_are_enforced() {
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
}
