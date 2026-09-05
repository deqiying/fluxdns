//! 独立 SQLite cache persistence adapter。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, Sqlite, SqlitePool};
use thiserror::Error;

use super::key::CACHE_KEY_FORMAT_VERSION;
use super::persistence::{
    CodecError, HEADER_BYTES, decode_record, encode_record, encode_storage_key,
};
use crate::dns::Deadline;
use crate::ports::cache::{
    CacheKey, CacheRecord, CacheRecoverySummary, PersistentCacheBatch, PersistentCacheStore,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

const SQLITE_BUSY_TIMEOUT_MS: u64 = 2_000;
const CACHE_SCHEMA_VERSION: u16 = 2;
const CACHE_FORMAT_VERSION: u16 = 2;
const LEGACY_CACHE_FORMAT_VERSION: u16 = 1;

#[derive(Clone)]
pub struct SqlitePersistentCacheStore {
    pool: SqlitePool,
    path: Arc<PathBuf>,
    max_size_bytes: u64,
    state: Arc<Mutex<SqliteState>>,
    operation_lock: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    injected_fault: Arc<Mutex<Option<InjectedSqliteFault>>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SqliteCacheDiskUsage {
    pub main_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
}

impl SqliteCacheDiskUsage {
    pub const fn total_bytes(self) -> u64 {
        self.main_bytes
            .saturating_add(self.wal_bytes)
            .saturating_add(self.shm_bytes)
    }
}

#[derive(Default)]
struct SqliteState {
    shutting_down: bool,
    invalid_rows: Vec<i64>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SqlitePersistentCacheStoreBuildError {
    #[error("sqlite cache max size must be greater than zero")]
    ZeroMaxSize,
    #[error("sqlite cache directory could not be prepared")]
    Directory,
    #[error("sqlite cache database could not be opened")]
    Connect,
    #[error("sqlite cache schema could not be initialized")]
    Schema,
}

struct DecodedRows {
    records: HashMap<crate::ports::cache::CacheKey, CacheRecord>,
    recovery: CacheRecoverySummary,
}

impl SqlitePersistentCacheStore {
    /// 打开独立 cache 数据库并初始化 schema。
    pub async fn connect(
        path: impl Into<PathBuf>,
        max_size_bytes: u64,
    ) -> Result<Self, SqlitePersistentCacheStoreBuildError> {
        if max_size_bytes == 0 {
            return Err(SqlitePersistentCacheStoreBuildError::ZeroMaxSize);
        }
        let path = path.into();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|_| SqlitePersistentCacheStoreBuildError::Directory)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(|_| SqlitePersistentCacheStoreBuildError::Connect)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS cache_entries (\
                id INTEGER PRIMARY KEY AUTOINCREMENT,\
                payload BLOB NOT NULL\
            )",
        )
        .execute(&pool)
        .await
        .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS cache_meta (\
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\
                schema_version INTEGER NOT NULL,\
                cache_format_version INTEGER NOT NULL,\
                key_format_version INTEGER NOT NULL\
            )",
        )
        .execute(&pool)
        .await
        .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
        let metadata = sqlx::query(
            "SELECT schema_version, cache_format_version, key_format_version \
             FROM cache_meta WHERE singleton = 1",
        )
        .fetch_optional(&pool)
        .await
        .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
        if let Some(metadata) = metadata {
            let schema_version = metadata
                .try_get::<i64, _>("schema_version")
                .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
            let cache_format_version = metadata
                .try_get::<i64, _>("cache_format_version")
                .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
            let key_format_version = metadata
                .try_get::<i64, _>("key_format_version")
                .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
            let supported_schema =
                schema_version == 1 || schema_version == i64::from(CACHE_SCHEMA_VERSION);
            let current = supported_schema
                && cache_format_version == i64::from(CACHE_FORMAT_VERSION)
                && key_format_version == i64::from(CACHE_KEY_FORMAT_VERSION);
            let provenance_upgrade = supported_schema
                && cache_format_version == i64::from(LEGACY_CACHE_FORMAT_VERSION)
                && key_format_version == i64::from(CACHE_KEY_FORMAT_VERSION);
            if provenance_upgrade {
                let mut transaction = pool
                    .begin()
                    .await
                    .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
                sqlx::query("DELETE FROM cache_entries")
                    .execute(&mut *transaction)
                    .await
                    .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
                sqlx::query("UPDATE cache_meta SET cache_format_version = ? WHERE singleton = 1")
                    .bind(i64::from(CACHE_FORMAT_VERSION))
                    .execute(&mut *transaction)
                    .await
                    .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
                transaction
                    .commit()
                    .await
                    .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
            } else if !current {
                pool.close().await;
                return Err(SqlitePersistentCacheStoreBuildError::Schema);
            }
        } else {
            sqlx::query(
                "INSERT INTO cache_meta \
                 (singleton, schema_version, cache_format_version, key_format_version) \
                 VALUES (1, ?, ?, ?)",
            )
            .bind(1_i64)
            .bind(i64::from(CACHE_FORMAT_VERSION))
            .bind(i64::from(CACHE_KEY_FORMAT_VERSION))
            .execute(&pool)
            .await
            .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
        }
        migrate_incremental_schema(&pool).await?;
        Ok(Self {
            pool,
            path: Arc::new(path),
            max_size_bytes,
            state: Arc::new(Mutex::new(SqliteState::default())),
            operation_lock: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            injected_fault: Arc::new(Mutex::new(None)),
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub const fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    /// 返回主数据库及 SQLite WAL/SHM sidecar 的当前磁盘占用。
    pub fn disk_usage(&self) -> Result<SqliteCacheDiskUsage, PortError> {
        self.available("sqlite_cache.disk_usage")?;
        Ok(SqliteCacheDiskUsage {
            main_bytes: file_size(self.path.as_ref(), "sqlite_cache.disk_usage")?,
            wal_bytes: file_size(
                &sidecar_path(self.path.as_ref(), "wal"),
                "sqlite_cache.disk_usage",
            )?,
            shm_bytes: file_size(
                &sidecar_path(self.path.as_ref(), "shm"),
                "sqlite_cache.disk_usage",
            )?,
        })
    }

    #[cfg(test)]
    fn inject_fault(&self, fault: InjectedSqliteFault) {
        *self
            .injected_fault
            .lock()
            .expect("sqlite cache injected fault lock must not be poisoned") = Some(fault);
    }

    #[cfg(test)]
    fn take_injected_fault(&self) -> Option<InjectedSqliteFault> {
        self.injected_fault
            .lock()
            .expect("sqlite cache injected fault lock must not be poisoned")
            .take()
    }

    fn available(&self, operation: &'static str) -> Result<(), PortError> {
        let state = self
            .state
            .lock()
            .map_err(|_| PortError::new(PortErrorClass::Internal, operation))?;
        if state.shutting_down {
            Err(PortError::new(PortErrorClass::Unavailable, operation))
        } else {
            Ok(())
        }
    }

    /// 在 deadline 内取得串行 operation lock，避免数据库排队无限越过调用方预算。
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

    async fn read_rows(
        &self,
        now: Instant,
        operation: &'static str,
    ) -> Result<DecodedRows, PortError> {
        let rows = sqlx::query("SELECT id, payload, inserted_at FROM cache_entries ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(|error| database_error(error, operation))?;
        let mut records = HashMap::with_capacity(rows.len());
        let wall_now = unix_now_nanos();
        let mut recovery = CacheRecoverySummary::default();
        let mut invalid_rows = Vec::new();
        for row in rows {
            let payload = row
                .try_get::<Vec<u8>, _>("payload")
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
            match decode_record(&payload, now) {
                Ok((key, mut record)) if visible(&record, now) => {
                    if let Some(inserted_at) = row
                        .try_get::<Option<i64>, _>("inserted_at")
                        .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?
                    {
                        // 增量写不会刷新旧 payload 的 age，恢复时以独立绝对时间保持原插入年龄。
                        if let Some(entry) = Arc::get_mut(&mut record.entry) {
                            entry.inserted_at = now
                                .checked_sub(Duration::from_nanos(
                                    wall_now.saturating_sub(inserted_at).max(0) as u64,
                                ))
                                .unwrap_or(now);
                        }
                    }
                    recovery.loaded = recovery.loaded.saturating_add(1);
                    records.insert(key, record);
                }
                Ok(_) => {
                    recovery.expired = recovery.expired.saturating_add(1);
                }
                Err(CodecError::Incompatible) => {
                    recovery.incompatible = recovery.incompatible.saturating_add(1);
                    invalid_rows.push(row.get::<i64, _>("id"));
                }
                Err(_) => {
                    recovery.corrupt = recovery.corrupt.saturating_add(1);
                    invalid_rows.push(row.get::<i64, _>("id"));
                }
            }
        }
        self.state
            .lock()
            .map_err(|_| PortError::new(PortErrorClass::Internal, operation))?
            .invalid_rows = invalid_rows;
        Ok(DecodedRows { records, recovery })
    }

    /// 一个批次的 upsert、失效清理和预算淘汰属于同一事务；失败不覆盖上一批有效内容。
    async fn write_records(
        &self,
        records: Vec<(CacheKey, CacheRecord)>,
        now: Instant,
        operation: &'static str,
    ) -> Result<(), PortError> {
        #[cfg(test)]
        if let Some(fault) = self.take_injected_fault() {
            return Err(PortError::new(PortErrorClass::Unavailable, operation)
                .with_safe_context(fault.safe_context()));
        }
        let wall_now = unix_now_nanos();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| database_error(error, operation))?;
        self.remove_known_invalid(&mut transaction, operation)
            .await?;
        // 同一批的重复 key 与原 HashMap 合并一致，最后一个值有效。
        for (key, record) in records.into_iter().collect::<HashMap<_, _>>() {
            let storage_key =
                encode_storage_key(&key).map_err(|error| codec_error(error, operation))?;
            if !visible(&record, now) {
                sqlx::query("DELETE FROM cache_entries WHERE entry_key = ?")
                    .bind(storage_key)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| database_error(error, operation))?;
                continue;
            }
            let payload =
                encode_record(&key, &record, now).map_err(|error| codec_error(error, operation))?;
            let size = payload.len() as u64 + 4;
            if size.saturating_add(HEADER_BYTES) > self.max_size_bytes {
                return Err(PortError::new(PortErrorClass::ResourceExhausted, operation));
            }
            sqlx::query(
                "INSERT INTO cache_entries (entry_key, payload, encoded_size, inserted_at, \
                 visible_until, version, sort_key) VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(entry_key) DO UPDATE SET payload=excluded.payload, \
                 encoded_size=excluded.encoded_size, inserted_at=excluded.inserted_at, \
                 visible_until=excluded.visible_until, version=excluded.version, sort_key=excluded.sort_key",
            )
            .bind(storage_key).bind(payload).bind(size as i64)
            .bind(wall_from_instant(record.entry.inserted_at, now, wall_now))
            .bind(wall_from_instant(visible_until(&record), now, wall_now))
            .bind(record.version.0.to_be_bytes().to_vec())
            .bind(key.encoded.as_ref())
            .execute(&mut *transaction).await
            .map_err(|error| database_error(error, operation))?;
        }
        trim_records(&mut transaction, self.max_size_bytes, wall_now, operation).await?;
        transaction
            .commit()
            .await
            .map_err(|error| database_error(error, operation))?;
        self.state
            .lock()
            .map_err(|_| PortError::new(PortErrorClass::Internal, operation))?
            .invalid_rows
            .clear();
        Ok(())
    }

    async fn remove_known_invalid(
        &self,
        transaction: &mut sqlx::Transaction<'_, Sqlite>,
        operation: &'static str,
    ) -> Result<u64, PortError> {
        let invalid = self
            .state
            .lock()
            .map_err(|_| PortError::new(PortErrorClass::Internal, operation))?
            .invalid_rows
            .clone();
        let mut removed = 0;
        for id in invalid {
            removed += sqlx::query("DELETE FROM cache_entries WHERE id = ?")
                .bind(id)
                .execute(&mut **transaction)
                .await
                .map_err(|error| database_error(error, operation))?
                .rows_affected();
        }
        Ok(removed)
    }
}

/// schema 1 仅有 payload。一次性事务回填索引，保留有效记录及原 id；未知格式不猜测。
async fn migrate_incremental_schema(
    pool: &SqlitePool,
) -> Result<(), SqlitePersistentCacheStoreBuildError> {
    let result = async {
        let mut transaction = pool.begin().await?;
        let schema: i64 = sqlx::query_scalar("SELECT schema_version FROM cache_meta WHERE singleton = 1")
            .fetch_one(&mut *transaction).await?;
        if schema == i64::from(CACHE_SCHEMA_VERSION) {
            return Ok::<(), sqlx::Error>(());
        }
        for statement in [
            "ALTER TABLE cache_entries ADD COLUMN entry_key BLOB",
            "ALTER TABLE cache_entries ADD COLUMN encoded_size INTEGER",
            "ALTER TABLE cache_entries ADD COLUMN inserted_at INTEGER",
            "ALTER TABLE cache_entries ADD COLUMN visible_until INTEGER",
            "ALTER TABLE cache_entries ADD COLUMN version BLOB",
            "ALTER TABLE cache_entries ADD COLUMN sort_key BLOB",
            "CREATE UNIQUE INDEX cache_entry_key ON cache_entries(entry_key)",
            "CREATE INDEX cache_entry_expiry ON cache_entries(visible_until)",
            "CREATE INDEX cache_entry_eviction ON cache_entries(inserted_at, version, sort_key, entry_key, encoded_size)",
        ] {
            sqlx::query(statement).execute(&mut *transaction).await?;
        }
        let now = Instant::now();
        let wall_now = unix_now_nanos();
        let rows = sqlx::query("SELECT id, payload FROM cache_entries ORDER BY id")
            .fetch_all(&mut *transaction).await?;
        for row in rows {
            let id: i64 = row.try_get("id")?;
            let payload: Vec<u8> = row.try_get("payload")?;
            let Ok((key, record)) = decode_record(&payload, now) else {
                // 损坏项留给 recover 计数和 maintain 清理，不伪装成有效缓存。
                continue;
            };
            let storage_key = encode_storage_key(&key).map_err(|_| sqlx::Error::Protocol("invalid cache key".into()))?;
            // 与旧 recover 的 ORDER BY id + HashMap 覆盖保持一致，重复 key 保留最后一条。
            sqlx::query("DELETE FROM cache_entries WHERE entry_key = ?")
                .bind(&storage_key).execute(&mut *transaction).await?;
            sqlx::query(
                "UPDATE cache_entries SET entry_key=?, encoded_size=?, inserted_at=?, \
                 visible_until=?, version=?, sort_key=? WHERE id=?",
            )
            .bind(storage_key).bind(payload.len() as i64 + 4)
            .bind(wall_from_instant(record.entry.inserted_at, now, wall_now))
            .bind(wall_from_instant(visible_until(&record), now, wall_now))
            .bind(record.version.0.to_be_bytes().to_vec()).bind(key.encoded.as_ref()).bind(id)
            .execute(&mut *transaction).await?;
        }
        sqlx::query("UPDATE cache_meta SET schema_version = ? WHERE singleton = 1")
            .bind(i64::from(CACHE_SCHEMA_VERSION)).execute(&mut *transaction).await?;
        transaction.commit().await
    }.await;
    result.map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)
}

fn unix_now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

fn wall_from_instant(value: Instant, now: Instant, wall_now: i64) -> i64 {
    if value >= now {
        wall_now.saturating_add(value.duration_since(now).as_nanos().min(i64::MAX as u128) as i64)
    } else {
        wall_now.saturating_sub(now.duration_since(value).as_nanos().min(i64::MAX as u128) as i64)
    }
}

fn visible_until(record: &CacheRecord) -> Instant {
    record
        .entry
        .stale_until
        .unwrap_or(record.entry.expires_at)
        .max(record.entry.expires_at)
}

/// 常规批次只扫描小型容量索引，不读取/解码未变化 payload；超额时有界读取最旧元数据。
async fn trim_records(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    max_size_bytes: u64,
    wall_now: i64,
    operation: &'static str,
) -> Result<u64, PortError> {
    let mut removed =
        sqlx::query("DELETE FROM cache_entries WHERE entry_key IS NULL OR visible_until <= ?")
            .bind(wall_now)
            .execute(&mut **transaction)
            .await
            .map_err(|error| database_error(error, operation))?
            .rows_affected();
    let size: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(encoded_size), 0) FROM cache_entries INDEXED BY cache_entry_eviction",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| database_error(error, operation))?;
    let mut total = (size as u64).saturating_add(HEADER_BYTES);
    while total > max_size_bytes {
        let rows = sqlx::query(
            "SELECT entry_key, encoded_size FROM cache_entries \
             ORDER BY inserted_at, version, sort_key, entry_key LIMIT 256",
        )
        .fetch_all(&mut **transaction)
        .await
        .map_err(|error| database_error(error, operation))?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let key: Vec<u8> = row
                .try_get("entry_key")
                .map_err(|error| database_error(error, operation))?;
            let size: i64 = row
                .try_get("encoded_size")
                .map_err(|error| database_error(error, operation))?;
            removed += sqlx::query("DELETE FROM cache_entries WHERE entry_key = ?")
                .bind(key)
                .execute(&mut **transaction)
                .await
                .map_err(|error| database_error(error, operation))?
                .rows_affected();
            total = total.saturating_sub(size as u64);
            if total <= max_size_bytes {
                break;
            }
        }
    }
    Ok(removed)
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push("-");
    value.push(suffix);
    PathBuf::from(value)
}

fn file_size(path: &Path, operation: &'static str) -> Result<u64, PortError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(_) => Err(PortError::new(PortErrorClass::Unavailable, operation)),
    }
}

impl std::fmt::Debug for SqlitePersistentCacheStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqlitePersistentCacheStore")
            .field("path", &self.path)
            .field("max_size_bytes", &self.max_size_bytes)
            .finish_non_exhaustive()
    }
}

fn visible(record: &CacheRecord, now: Instant) -> bool {
    now < record.entry.expires_at
        || record
            .entry
            .stale_until
            .is_some_and(|stale_until| now < stale_until)
}

fn codec_error(error: CodecError, operation: &'static str) -> PortError {
    let class = match error {
        CodecError::Corrupt => PortErrorClass::CorruptData,
        CodecError::Incompatible => PortErrorClass::Unavailable,
        CodecError::ResourceExhausted => PortErrorClass::ResourceExhausted,
    };
    PortError::new(class, operation)
}

fn database_error(error: sqlx::Error, operation: &'static str) -> PortError {
    let class = match error {
        sqlx::Error::PoolClosed | sqlx::Error::Io(_) => PortErrorClass::Unavailable,
        sqlx::Error::Database(database) if database.is_unique_violation() => {
            PortErrorClass::CorruptData
        }
        sqlx::Error::Database(_) => PortErrorClass::Unavailable,
        _ => PortErrorClass::Internal,
    };
    PortError::new(class, operation)
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

impl PersistentCacheStore for SqlitePersistentCacheStore {
    fn recover(
        &self,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(PersistentCacheBatch, CacheRecoverySummary), PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_cache.recover",
            async move {
                self.available("sqlite_cache.recover")?;
                let _guard = self
                    .lock_operation(deadline, "sqlite_cache.recover")
                    .await?;
                let decoded = self
                    .read_rows(Instant::now(), "sqlite_cache.recover")
                    .await?;
                Ok((
                    PersistentCacheBatch {
                        records: decoded.records.into_iter().collect(),
                    },
                    decoded.recovery,
                ))
            },
        ))
    }

    fn persist(
        &self,
        batch: PersistentCacheBatch,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_cache.persist",
            async move {
                self.available("sqlite_cache.persist")?;
                let _guard = self
                    .lock_operation(deadline, "sqlite_cache.persist")
                    .await?;
                let now = Instant::now();
                self.write_records(batch.records, now, "sqlite_cache.persist")
                    .await
            },
        ))
    }

    fn maintain_capacity(&self, deadline: Deadline) -> PortFuture<'_, Result<u64, PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_cache.maintain_capacity",
            async move {
                self.available("sqlite_cache.maintain_capacity")?;
                let _guard = self
                    .lock_operation(deadline, "sqlite_cache.maintain_capacity")
                    .await?;
                let operation = "sqlite_cache.maintain_capacity";
                let mut transaction = self
                    .pool
                    .begin()
                    .await
                    .map_err(|error| database_error(error, operation))?;
                let corrupt = self
                    .remove_known_invalid(&mut transaction, operation)
                    .await?;
                let removed = trim_records(
                    &mut transaction,
                    self.max_size_bytes,
                    unix_now_nanos(),
                    operation,
                )
                .await?;
                transaction
                    .commit()
                    .await
                    .map_err(|error| database_error(error, operation))?;
                self.state
                    .lock()
                    .map_err(|_| PortError::new(PortErrorClass::Internal, operation))?
                    .invalid_rows
                    .clear();
                Ok(removed.saturating_add(corrupt))
            },
        ))
    }

    fn shutdown(&self, deadline: Deadline) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(run_with_deadline(
            deadline,
            "sqlite_cache.shutdown",
            async move {
                let _guard = self
                    .lock_operation(deadline, "sqlite_cache.shutdown")
                    .await?;
                {
                    let mut state = self.state.lock().map_err(|_| {
                        PortError::new(PortErrorClass::Internal, "sqlite_cache.shutdown")
                    })?;
                    state.shutting_down = true;
                }
                self.pool.close().await;
                Ok(())
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};
    use sqlx::Row;

    use super::{
        CACHE_FORMAT_VERSION, CACHE_KEY_FORMAT_VERSION, CACHE_SCHEMA_VERSION, InjectedSqliteFault,
        SqlitePersistentCacheStore, SqlitePersistentCacheStoreBuildError,
    };
    use crate::dns::{CanonicalQuery, CanonicalResponse, Deadline, DnsMessageId, RuntimeRevision};
    use crate::ports::cache::{
        CacheEntry, CacheKey, CacheNamespace, CacheQuality, CacheRecord, CacheResponseClass,
        CacheVersion, PersistentCacheBatch, PersistentCacheStore,
    };

    static NEXT_TEST_DB: AtomicU64 = AtomicU64::new(0);

    fn db_path() -> std::path::PathBuf {
        let id = NEXT_TEST_DB.fetch_add(1, Ordering::Relaxed);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/tests/cache");
        std::fs::create_dir_all(&root).unwrap();
        root.join(format!("fluxdns-cache-{id}-{}.sqlite3", std::process::id()))
    }

    #[tokio::test]
    async fn incremental_batch_preserves_unchanged_rows_and_only_writes_changed_keys() {
        let store = SqlitePersistentCacheStore::connect(db_path(), 128 * 1024)
            .await
            .unwrap();
        let records = (0..50)
            .map(|index| {
                (
                    key(format!("entry-{index:02}").as_bytes()),
                    record(Duration::from_secs(60)),
                )
            })
            .collect();
        store
            .persist(PersistentCacheBatch { records }, deadline())
            .await
            .unwrap();
        let untouched_key = super::encode_storage_key(&key(b"entry-00")).unwrap();
        let before: (i64, Vec<u8>) =
            sqlx::query_as("SELECT id, payload FROM cache_entries WHERE entry_key=?")
                .bind(&untouched_key)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        sqlx::query("CREATE TABLE write_audit (kind TEXT)")
            .execute(&store.pool)
            .await
            .unwrap();
        for sql in [
            "CREATE TRIGGER audit_insert AFTER INSERT ON cache_entries BEGIN INSERT INTO write_audit VALUES ('INSERT'); END",
            "CREATE TRIGGER audit_update AFTER UPDATE ON cache_entries BEGIN INSERT INTO write_audit VALUES ('UPDATE'); END",
            "CREATE TRIGGER audit_delete AFTER DELETE ON cache_entries BEGIN INSERT INTO write_audit VALUES ('DELETE'); END",
        ] {
            sqlx::query(sql).execute(&store.pool).await.unwrap();
        }
        let mut changed = record(Duration::from_secs(60));
        changed.version = CacheVersion(2);
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![
                        (key(b"entry-01"), changed.clone()),
                        (key(b"entry-50"), changed),
                    ],
                },
                deadline(),
            )
            .await
            .unwrap();
        let after: (i64, Vec<u8>) =
            sqlx::query_as("SELECT id, payload FROM cache_entries WHERE entry_key=?")
                .bind(&untouched_key)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            before, after,
            "unchanged payload and row id must not be rewritten"
        );
        let writes: Vec<(String, i64)> =
            sqlx::query_as("SELECT kind, COUNT(*) FROM write_audit GROUP BY kind ORDER BY kind")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            writes,
            vec![("INSERT".to_owned(), 1), ("UPDATE".to_owned(), 1)]
        );
        assert_eq!(store.recover(deadline()).await.unwrap().1.loaded, 51);
        assert_eq!(store.maintain_capacity(deadline()).await.unwrap(), 0);
        let writes_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM write_audit")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(writes_after, 2);
        store.shutdown(deadline()).await.unwrap();
    }

    #[tokio::test]
    async fn schema_one_upgrade_preserves_payload_and_last_duplicate_without_clearing_cache() {
        let path = db_path();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE cache_entries (id INTEGER PRIMARY KEY AUTOINCREMENT, payload BLOB NOT NULL)")
            .execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE cache_meta (singleton INTEGER PRIMARY KEY, schema_version INTEGER, cache_format_version INTEGER, key_format_version INTEGER)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO cache_meta VALUES (1, 1, ?, ?)")
            .bind(i64::from(CACHE_FORMAT_VERSION))
            .bind(i64::from(CACHE_KEY_FORMAT_VERSION))
            .execute(&pool)
            .await
            .unwrap();
        let key = key(b"old-format");
        let mut record = record(Duration::from_secs(60));
        let first = super::encode_record(&key, &record, Instant::now()).unwrap();
        record.version = CacheVersion(2);
        let latest = super::encode_record(&key, &record, Instant::now()).unwrap();
        for payload in [&first, &latest, &vec![99_u8]] {
            sqlx::query("INSERT INTO cache_entries(payload) VALUES (?)")
                .bind(payload)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        let (batch, summary) = store.recover(deadline()).await.unwrap();
        assert_eq!(summary.loaded, 1);
        assert_eq!(summary.corrupt, 1);
        assert_eq!(batch.records[0].1.version, CacheVersion(2));
        let row: (i64, Vec<u8>) =
            sqlx::query_as("SELECT id, payload FROM cache_entries WHERE entry_key IS NOT NULL")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(row, (2, latest));
        assert_eq!(store.maintain_capacity(deadline()).await.unwrap(), 1);
        store.shutdown(deadline()).await.unwrap();
        let reopened = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        assert_eq!(reopened.recover(deadline()).await.unwrap().1.loaded, 1);
        reopened.shutdown(deadline()).await.unwrap();
    }

    #[tokio::test]
    async fn failed_incremental_write_rolls_back_and_recovered_corruption_is_removed() {
        let store = SqlitePersistentCacheStore::connect(db_path(), 16 * 1024)
            .await
            .unwrap();
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![(key(b"baseline"), record(Duration::from_secs(60)))],
                },
                deadline(),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER reject_insert BEFORE INSERT ON cache_entries BEGIN SELECT RAISE(ABORT, 'test failure'); END")
            .execute(&store.pool).await.unwrap();
        assert!(
            store
                .persist(
                    PersistentCacheBatch {
                        records: vec![(key(b"new"), record(Duration::from_secs(60)))]
                    },
                    deadline()
                )
                .await
                .is_err()
        );
        assert_eq!(store.recover(deadline()).await.unwrap().1.loaded, 1);
        sqlx::query("DROP TRIGGER reject_insert")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE cache_entries SET payload = X'63'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.recover(deadline()).await.unwrap().1.corrupt, 1);
        assert_eq!(store.maintain_capacity(deadline()).await.unwrap(), 1);
        store.shutdown(deadline()).await.unwrap();
    }

    #[tokio::test]
    async fn incremental_keys_keep_namespaces_distinct_and_restore_absolute_insertion_age() {
        let store = SqlitePersistentCacheStore::connect(db_path(), 16 * 1024)
            .await
            .unwrap();
        let global = key(b"same-wire");
        let mut strategy = global.clone();
        strategy.namespace = CacheNamespace::Strategy(
            crate::ports::cache::CacheStrategyId::from_validated_config_id("strategy").unwrap(),
        );
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![
                        (global.clone(), record(Duration::from_secs(60))),
                        (strategy.clone(), record(Duration::from_secs(60))),
                    ],
                },
                deadline(),
            )
            .await
            .unwrap();
        // 只推进索引中的绝对年龄，证明恢复不再使用未改写 payload 的静态 age。
        sqlx::query(
            "UPDATE cache_entries SET inserted_at = inserted_at - 5000000000 WHERE entry_key = ?",
        )
        .bind(super::encode_storage_key(&global).unwrap())
        .execute(&store.pool)
        .await
        .unwrap();
        let (restored, summary) = store.recover(deadline()).await.unwrap();
        assert_eq!(summary.loaded, 2);
        let old = restored
            .records
            .iter()
            .find(|(key, _)| key == &global)
            .unwrap();
        let other = restored
            .records
            .iter()
            .find(|(key, _)| key == &strategy)
            .unwrap();
        assert!(other.1.entry.inserted_at > old.1.entry.inserted_at + Duration::from_secs(4));
        store.shutdown(deadline()).await.unwrap();
    }

    fn key(value: &[u8]) -> CacheKey {
        CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(value),
            format_version: CACHE_KEY_FORMAT_VERSION,
        }
    }

    fn record(ttl: Duration) -> CacheRecord {
        let now = Instant::now();
        let name = Name::from_ascii("example.com.").unwrap();
        let question = Query::query(name, RecordType::A);
        let mut query_message = Message::new(0, MessageType::Query, OpCode::Query);
        query_message.add_query(question.clone());
        let query = CanonicalQuery::from_message(query_message).unwrap();
        let mut response_message = Message::response(0, OpCode::Query);
        response_message.add_query(question);
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(0))
                .unwrap();
        let checksum = crate::cache::canonical_checksum(&response).unwrap();
        CacheRecord {
            version: CacheVersion(1),
            entry: Arc::new(CacheEntry {
                response: Arc::new(response),
                upstream:
                    crate::ports::cache::CacheUpstreamProvenance::direct_from_validated_config_id(
                        "test-upstream",
                    )
                    .unwrap(),
                inserted_at: now,
                expires_at: now + ttl,
                stale_until: None,
                response_class: CacheResponseClass::NoData,
                producer_revision: RuntimeRevision(1),
                quality: CacheQuality::Negative,
                checksum,
                format_version: crate::ports::cache::CACHE_ENTRY_FORMAT_VERSION,
            }),
        }
    }

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    #[tokio::test]
    async fn roundtrip_and_expiry_recovery_are_isolated_per_database() {
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        let metadata = sqlx::query("SELECT schema_version, cache_format_version, key_format_version FROM cache_meta WHERE singleton = 1")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            metadata.get::<i64, _>("schema_version"),
            i64::from(CACHE_SCHEMA_VERSION)
        );
        assert_eq!(
            metadata.get::<i64, _>("cache_format_version"),
            i64::from(CACHE_FORMAT_VERSION)
        );
        assert_eq!(
            metadata.get::<i64, _>("key_format_version"),
            i64::from(CACHE_KEY_FORMAT_VERSION)
        );
        let usage = store.disk_usage().unwrap();
        assert!(usage.main_bytes > 0);
        assert_eq!(
            usage.total_bytes(),
            usage
                .main_bytes
                .saturating_add(usage.wal_bytes)
                .saturating_add(usage.shm_bytes)
        );
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![(key(b"live"), record(Duration::from_secs(60)))],
                },
                deadline(),
            )
            .await
            .unwrap();
        let recovered = store.recover(deadline()).await.unwrap();
        assert_eq!(recovered.0.records.len(), 1);
        assert_eq!(recovered.1.loaded, 1);
        store.shutdown(deadline()).await.unwrap();
        let reopened = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        let recovered = reopened.recover(deadline()).await.unwrap();
        assert_eq!(recovered.0.records.len(), 1);
        reopened.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn shutdown_rejects_recovery_and_zero_budget_is_rejected() {
        assert!(
            SqlitePersistentCacheStore::connect(db_path(), 0)
                .await
                .is_err()
        );
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        store.shutdown(deadline()).await.unwrap();
        assert!(store.recover(deadline()).await.is_err());
        assert!(store.disk_usage().is_err());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn capacity_trims_oldest_records_and_recovery_isolates_corruption() {
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 420)
            .await
            .unwrap();
        let records = (0..6)
            .map(|index| {
                (
                    key(format!("entry-{index}").as_bytes()),
                    record(Duration::from_secs(60)),
                )
            })
            .collect();
        store
            .persist(PersistentCacheBatch { records }, deadline())
            .await
            .unwrap();
        let recovered = store.recover(deadline()).await.unwrap();
        assert!(recovered.0.records.len() < 6);
        sqlx::query("INSERT INTO cache_entries (payload) VALUES (?)")
            .bind(vec![99_u8])
            .execute(&store.pool)
            .await
            .unwrap();
        let recovered = store.recover(deadline()).await.unwrap();
        assert_eq!(recovered.1.corrupt, 1);
        store.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn injected_busy_and_disk_full_faults_are_retriable() {
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        let mut expected_loaded = 0;
        for (fault, version) in [
            (InjectedSqliteFault::Busy, 10),
            (InjectedSqliteFault::DiskFull, 11),
        ] {
            let item = (
                key(format!("fault-{version}").as_bytes()),
                record(Duration::from_secs(60)),
            );
            store.inject_fault(fault);
            let error = store
                .persist(
                    PersistentCacheBatch {
                        records: vec![item.clone()],
                    },
                    deadline(),
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error.class(),
                crate::ports::PortErrorClass::Unavailable
            ));
            assert_eq!(
                store.recover(deadline()).await.unwrap().1.loaded,
                expected_loaded
            );
            store
                .persist(
                    PersistentCacheBatch {
                        records: vec![item],
                    },
                    deadline(),
                )
                .await
                .unwrap();
            expected_loaded += 1;
            assert_eq!(
                store.recover(deadline()).await.unwrap().1.loaded,
                expected_loaded
            );
        }
        store.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// 通过真实 SQLite 写锁验证 Busy 不会破坏旧快照，释放锁后允许重试。
    #[tokio::test]
    async fn real_sqlite_write_lock_preserves_snapshot_and_allows_retry() {
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![(key(b"baseline"), record(Duration::from_secs(60)))],
                },
                deadline(),
            )
            .await
            .unwrap();

        let mut lock_connection = store.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *lock_connection)
            .await
            .unwrap();
        let retry_record = (key(b"retry"), record(Duration::from_secs(60)));
        let error = store
            .persist(
                PersistentCacheBatch {
                    records: vec![retry_record.clone()],
                },
                deadline(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Unavailable
        ));
        let unchanged = store.recover(deadline()).await.unwrap();
        assert_eq!(unchanged.1.loaded, 1);
        assert_eq!(unchanged.0.records[0].0.encoded.as_ref(), b"baseline");

        sqlx::query("ROLLBACK")
            .execute(&mut *lock_connection)
            .await
            .unwrap();
        drop(lock_connection);
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![retry_record],
                },
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(store.recover(deadline()).await.unwrap().1.loaded, 2);

        store.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// 验证串行数据库操作和 shutdown 在等待锁时遵守 deadline。
    #[tokio::test]
    async fn operation_lock_wait_honors_deadline() {
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        let guard = store.operation_lock.lock().await;

        let recover_deadline = Deadline::new(Instant::now() + Duration::from_millis(20));
        let recover_error = store.recover(recover_deadline).await.unwrap_err();
        assert!(matches!(
            recover_error.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        assert_eq!(recover_error.operation(), "sqlite_cache.recover");

        let shutdown_deadline = Deadline::new(Instant::now() + Duration::from_millis(20));
        let shutdown_error = store.shutdown(shutdown_deadline).await.unwrap_err();
        assert!(matches!(
            shutdown_error.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        assert_eq!(shutdown_error.operation(), "sqlite_cache.shutdown");

        drop(guard);
        assert_eq!(store.recover(deadline()).await.unwrap().1.loaded, 0);
        store.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    /// 验证 SQLite Busy 等待不会越过更短的调用方 deadline。
    #[tokio::test]
    async fn sqlite_busy_honors_short_caller_deadline() {
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![(key(b"baseline"), record(Duration::from_secs(60)))],
                },
                deadline(),
            )
            .await
            .unwrap();
        let mut lock_connection = store.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *lock_connection)
            .await
            .unwrap();

        let short_deadline = Deadline::new(Instant::now() + Duration::from_millis(20));
        let error = store
            .persist(
                PersistentCacheBatch {
                    records: vec![(key(b"timed-out"), record(Duration::from_secs(60)))],
                },
                short_deadline,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        assert_eq!(error.operation(), "sqlite_cache.persist");
        assert_eq!(store.recover(deadline()).await.unwrap().1.loaded, 1);

        sqlx::query("ROLLBACK")
            .execute(&mut *lock_connection)
            .await
            .unwrap();
        drop(lock_connection);
        store.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn known_v1_cache_format_is_cleared_and_upgraded_for_provenance() {
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![(key(b"legacy"), record(Duration::from_secs(60)))],
                },
                deadline(),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE cache_meta SET cache_format_version = 1 WHERE singleton = 1")
            .execute(&store.pool)
            .await
            .unwrap();
        store.shutdown(deadline()).await.unwrap();

        let upgraded = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        let format: i64 =
            sqlx::query_scalar("SELECT cache_format_version FROM cache_meta WHERE singleton = 1")
                .fetch_one(&upgraded.pool)
                .await
                .unwrap();
        let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cache_entries")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap();
        assert_eq!(format, i64::from(CACHE_FORMAT_VERSION));
        assert_eq!(entries, 0);

        upgraded.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn metadata_version_mismatch_rejects_the_cache_adapter() {
        let path = db_path();
        let store = SqlitePersistentCacheStore::connect(&path, 16 * 1024)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE cache_meta SET key_format_version = key_format_version + 1 WHERE singleton = 1",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        store.shutdown(deadline()).await.unwrap();

        assert!(matches!(
            SqlitePersistentCacheStore::connect(&path, 16 * 1024).await,
            Err(SqlitePersistentCacheStoreBuildError::Schema)
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
