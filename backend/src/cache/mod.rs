//! CacheStore 的内存实现。

mod admission;
mod key;
mod memory;
mod moka;
mod persistence;
mod runtime;
mod service;
mod sqlite;

pub use admission::{
    CacheAdmissionError, CacheAdmissionOutcome, CacheAdmissionPolicy, CacheAdmissionRejection,
    admit_response, canonical_checksum,
};
pub use key::{
    CACHE_KEY_FORMAT_VERSION, CacheFingerprint, CacheKeyDimensions, CacheKeyError, build_cache_key,
};
pub use memory::{MemoryCacheStore, MemoryCacheStoreBuildError};
pub use moka::{MokaCacheStore, MokaCacheStoreBuildError};
pub use persistence::{FilePersistentCacheStore, FilePersistentCacheStoreBuildError};
pub use runtime::{
    CachePersistenceEnqueueError, CachePersistenceRunSummary, CachePersistenceRuntime,
    CachePersistenceRuntimeBuildError, CachePersistenceWriter,
};
pub use service::{
    CacheFacade, CacheFacadeBuildError, CacheFacadeError, CacheFacadeOptions, CacheLookup,
    CacheRefreshPermit, CacheWriteRequest, CacheWriteResult, LateCacheFinalizer,
    LateCacheFinalizerBuildError, LateCacheFinalizerShutdownSummary, LateCacheFinalizerSubmitError,
};
pub use sqlite::{
    SqliteCacheDiskUsage, SqlitePersistentCacheStore, SqlitePersistentCacheStoreBuildError,
};

#[cfg(test)]
mod backend_contract_tests;

#[cfg(test)]
mod persistence_contract_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use super::{FilePersistentCacheStore, SqlitePersistentCacheStore, canonical_checksum};
    use crate::dns::{CanonicalQuery, CanonicalResponse, Deadline, DnsMessageId, RuntimeRevision};
    use crate::ports::cache::{
        CacheEntry, CacheKey, CacheNamespace, CacheQuality, CacheRecord, CacheResponseClass,
        CacheVersion, PersistentCacheBatch, PersistentCacheStore,
    };

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn path(extension: &str) -> std::path::PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fluxdns-cache-contract-{}-{id}.{extension}",
            std::process::id()
        ))
    }

    fn record(key: &str, version: u64, expires_at: Instant) -> (CacheKey, CacheRecord) {
        let mut query_message = Message::new(0, MessageType::Query, OpCode::Query);
        query_message.add_query(Query::query(
            Name::from_ascii("example.com.").expect("test name is valid"),
            RecordType::A,
        ));
        let query =
            CanonicalQuery::from_message(query_message.clone()).expect("test query is valid");
        let mut response_message = Message::response(0, OpCode::Query);
        response_message.add_query(query_message.queries[0].clone());
        let response = Arc::new(
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(0))
                .expect("test response is valid"),
        );
        let checksum = canonical_checksum(response.as_ref()).expect("test response is encodable");
        (
            CacheKey {
                namespace: CacheNamespace::Global,
                encoded: Arc::from(key.as_bytes()),
                format_version: 1,
            },
            CacheRecord {
                version: CacheVersion(version),
                entry: Arc::new(CacheEntry {
                    response,
                    inserted_at: Instant::now(),
                    expires_at,
                    stale_until: None,
                    response_class: CacheResponseClass::NoData,
                    producer_revision: RuntimeRevision(version),
                    quality: CacheQuality::Negative,
                    checksum,
                    format_version: 1,
                }),
            },
        )
    }

    async fn assert_contract(store: &dyn PersistentCacheStore) {
        let live = record("live", 1, Instant::now() + Duration::from_secs(30));
        let expired = record("expired", 2, Instant::now() + Duration::from_millis(20));
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![live.clone(), expired],
                },
                deadline(),
            )
            .await
            .expect("persistence succeeds");
        tokio::time::sleep(Duration::from_millis(40)).await;

        assert_eq!(store.maintain_capacity(deadline()).await.unwrap(), 1);
        let (batch, summary) = store.recover(deadline()).await.expect("recovery succeeds");
        assert_eq!(summary.loaded, 1);
        assert_eq!(summary.expired, 0);
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].0, live.0);
        assert_eq!(batch.records[0].1.version, live.1.version);
        assert_eq!(store.maintain_capacity(deadline()).await.unwrap(), 0);

        store.shutdown(deadline()).await.expect("shutdown succeeds");
        assert!(store.recover(deadline()).await.is_err());
        assert!(
            store
                .persist(
                    PersistentCacheBatch {
                        records: vec![live]
                    },
                    deadline()
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn file_and_sqlite_adapters_follow_the_same_contract() {
        let file_path = path("bin");
        let file_store =
            FilePersistentCacheStore::new(&file_path, 1024 * 1024).expect("file store builds");
        assert_contract(&file_store).await;
        let _ = std::fs::remove_file(&file_path);

        let sqlite_path = path("sqlite3");
        let sqlite_store = SqlitePersistentCacheStore::connect(&sqlite_path, 1024 * 1024)
            .await
            .expect("sqlite store builds");
        assert_contract(&sqlite_store).await;
        let _ = std::fs::remove_file(&sqlite_path);
        let _ = std::fs::remove_file(sqlite_path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(sqlite_path.with_extension("sqlite3-shm"));
    }
}
