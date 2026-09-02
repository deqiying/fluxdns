//! 独立 SQLite cache persistence adapter。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use super::key::CACHE_KEY_FORMAT_VERSION;
use super::persistence::{CodecError, decode_record, encode_record, prepare_snapshot};
use crate::dns::Deadline;
use crate::ports::cache::{
    CacheRecord, CacheRecoverySummary, PersistentCacheBatch, PersistentCacheStore,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

const SQLITE_BUSY_TIMEOUT_MS: u64 = 2_000;
const CACHE_SCHEMA_VERSION: u16 = 1;
const CACHE_FORMAT_VERSION: u16 = 1;

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
            if schema_version != i64::from(CACHE_SCHEMA_VERSION)
                || cache_format_version != i64::from(CACHE_FORMAT_VERSION)
                || key_format_version != i64::from(CACHE_KEY_FORMAT_VERSION)
            {
                pool.close().await;
                return Err(SqlitePersistentCacheStoreBuildError::Schema);
            }
        } else {
            sqlx::query(
                "INSERT INTO cache_meta \
                 (singleton, schema_version, cache_format_version, key_format_version) \
                 VALUES (1, ?, ?, ?)",
            )
            .bind(i64::from(CACHE_SCHEMA_VERSION))
            .bind(i64::from(CACHE_FORMAT_VERSION))
            .bind(i64::from(CACHE_KEY_FORMAT_VERSION))
            .execute(&pool)
            .await
            .map_err(|_| SqlitePersistentCacheStoreBuildError::Schema)?;
        }
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

    async fn read_rows(
        &self,
        now: Instant,
        operation: &'static str,
    ) -> Result<DecodedRows, PortError> {
        let rows = sqlx::query("SELECT payload FROM cache_entries ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(|error| database_error(error, operation))?;
        let mut records = HashMap::with_capacity(rows.len());
        let mut recovery = CacheRecoverySummary::default();
        for row in rows {
            let payload = row
                .try_get::<Vec<u8>, _>("payload")
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, operation))?;
            match decode_record(&payload, now) {
                Ok((key, record)) if visible(&record, now) => {
                    recovery.loaded = recovery.loaded.saturating_add(1);
                    records.insert(key, record);
                }
                Ok(_) => {
                    recovery.expired = recovery.expired.saturating_add(1);
                }
                Err(CodecError::Incompatible) => {
                    recovery.incompatible = recovery.incompatible.saturating_add(1);
                }
                Err(_) => {
                    recovery.corrupt = recovery.corrupt.saturating_add(1);
                }
            }
        }
        Ok(DecodedRows { records, recovery })
    }

    async fn write_records(
        &self,
        records: &HashMap<crate::ports::cache::CacheKey, CacheRecord>,
        now: Instant,
        operation: &'static str,
    ) -> Result<(), PortError> {
        #[cfg(test)]
        if let Some(fault) = self.take_injected_fault() {
            return Err(PortError::new(PortErrorClass::Unavailable, operation)
                .with_safe_context(fault.safe_context()));
        }
        let mut payloads = records
            .iter()
            .map(|(key, record)| encode_record(key, record, now))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| codec_error(error, operation))?;
        payloads.sort();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| database_error(error, operation))?;
        sqlx::query("DELETE FROM cache_entries")
            .execute(&mut *transaction)
            .await
            .map_err(|error| database_error(error, operation))?;
        for payload in payloads {
            sqlx::query("INSERT INTO cache_entries (payload) VALUES (?)")
                .bind(payload)
                .execute(&mut *transaction)
                .await
                .map_err(|error| database_error(error, operation))?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| database_error(error, operation))
    }

    async fn prepare_records(
        &self,
        mut records: HashMap<crate::ports::cache::CacheKey, CacheRecord>,
        now: Instant,
        operation: &'static str,
    ) -> Result<HashMap<crate::ports::cache::CacheKey, CacheRecord>, PortError> {
        let (kept, _snapshot) =
            prepare_snapshot(std::mem::take(&mut records), self.max_size_bytes, now)
                .map_err(|error| codec_error(error, operation))?;
        Ok(kept)
    }
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

impl PersistentCacheStore for SqlitePersistentCacheStore {
    fn recover(
        &self,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(PersistentCacheBatch, CacheRecoverySummary), PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(PortError::new(
                    PortErrorClass::Timeout,
                    "sqlite_cache.recover",
                ));
            }
            self.available("sqlite_cache.recover")?;
            let _guard = self.operation_lock.lock().await;
            let decoded = self
                .read_rows(Instant::now(), "sqlite_cache.recover")
                .await?;
            Ok((
                PersistentCacheBatch {
                    records: decoded.records.into_iter().collect(),
                },
                decoded.recovery,
            ))
        })
    }

    fn persist(
        &self,
        batch: PersistentCacheBatch,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(PortError::new(
                    PortErrorClass::Timeout,
                    "sqlite_cache.persist",
                ));
            }
            self.available("sqlite_cache.persist")?;
            let _guard = self.operation_lock.lock().await;
            let now = Instant::now();
            let mut records = self.read_rows(now, "sqlite_cache.persist").await?.records;
            records.extend(batch.records);
            let kept = self
                .prepare_records(records, now, "sqlite_cache.persist")
                .await?;
            self.write_records(&kept, now, "sqlite_cache.persist").await
        })
    }

    fn maintain_capacity(&self, deadline: Deadline) -> PortFuture<'_, Result<u64, PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(PortError::new(
                    PortErrorClass::Timeout,
                    "sqlite_cache.maintain_capacity",
                ));
            }
            self.available("sqlite_cache.maintain_capacity")?;
            let _guard = self.operation_lock.lock().await;
            let now = Instant::now();
            let decoded = self
                .read_rows(now, "sqlite_cache.maintain_capacity")
                .await?;
            let before = (decoded.records.len() as u64)
                .saturating_add(decoded.recovery.expired)
                .saturating_add(decoded.recovery.corrupt)
                .saturating_add(decoded.recovery.incompatible);
            let kept = self
                .prepare_records(decoded.records, now, "sqlite_cache.maintain_capacity")
                .await?;
            let removed = before.saturating_sub(kept.len() as u64);
            if removed > 0 {
                self.write_records(&kept, now, "sqlite_cache.maintain_capacity")
                    .await?;
            }
            Ok(removed)
        })
    }

    fn shutdown(&self, _deadline: Deadline) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let _guard = self.operation_lock.lock().await;
            {
                let mut state = self.state.lock().map_err(|_| {
                    PortError::new(PortErrorClass::Internal, "sqlite_cache.shutdown")
                })?;
                state.shutting_down = true;
            }
            self.pool.close().await;
            Ok(())
        })
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
        std::env::temp_dir().join(format!("fluxdns-cache-{id}-{}.sqlite3", std::process::id()))
    }

    fn key(value: &[u8]) -> CacheKey {
        CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(value),
            format_version: 1,
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
                inserted_at: now,
                expires_at: now + ttl,
                stale_until: None,
                response_class: CacheResponseClass::NoData,
                producer_revision: RuntimeRevision(1),
                quality: CacheQuality::Negative,
                checksum,
                format_version: 1,
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
