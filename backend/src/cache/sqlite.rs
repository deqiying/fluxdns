//! 独立 SQLite cache persistence adapter。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use super::persistence::{CodecError, decode_record, encode_record, prepare_snapshot};
use crate::dns::Deadline;
use crate::ports::cache::{
    CacheRecord, CacheRecoverySummary, PersistentCacheBatch, PersistentCacheStore,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

const SQLITE_BUSY_TIMEOUT_MS: u64 = 2_000;

#[derive(Clone)]
pub struct SqlitePersistentCacheStore {
    pool: SqlitePool,
    path: Arc<PathBuf>,
    max_size_bytes: u64,
    state: Arc<Mutex<SqliteState>>,
    operation_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct SqliteState {
    shutting_down: bool,
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
        Ok(Self {
            pool,
            path: Arc::new(path),
            max_size_bytes,
            state: Arc::new(Mutex::new(SqliteState::default())),
            operation_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub const fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
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
            let records = self
                .read_rows(now, "sqlite_cache.maintain_capacity")
                .await?
                .records;
            let before = records.len() as u64;
            let kept = self
                .prepare_records(records, now, "sqlite_cache.maintain_capacity")
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

    use super::SqlitePersistentCacheStore;
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
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
