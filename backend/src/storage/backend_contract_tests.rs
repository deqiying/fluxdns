//! 业务存储生产 adapter 共用的 `StorageBackend` 契约测试。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::dns::Deadline;
use crate::ports::PortErrorClass;
use crate::ports::storage::{
    SchemaVersion, StatsBatch, StatsEvent, StorageBackend, StorageHealth, StorageOperation,
    StorageTransaction,
};

use super::{InMemoryStorageBackend, STORAGE_SCHEMA_VERSION, SqliteStorageBackend};

/// 生成同时适用于内存与 SQLite adapter 的统计事务。
fn transaction(day_utc: i32) -> StorageTransaction {
    StorageTransaction {
        idempotency_key: Arc::from("storage-contract-batch-1"),
        operations: vec![StorageOperation::StatsBatch(StatsBatch {
            batch_id: 1,
            max_event_sequence: 1,
            counter_epoch: 0,
            events: vec![StatsEvent::new(1, day_utc, vec![]).unwrap()],
        })],
    }
}

/// 返回留有充足余量的测试 deadline。
fn deadline() -> Deadline {
    Deadline::new(Instant::now() + Duration::from_secs(5))
}

/// 对任意业务存储 adapter 执行相同的可观测行为断言。
async fn assert_storage_backend_contract(backend: &dyn StorageBackend) {
    let expired = Deadline::new(Instant::now() - Duration::from_millis(1));
    let error = backend
        .migrate(STORAGE_SCHEMA_VERSION, expired)
        .await
        .unwrap_err();
    assert!(matches!(error.class(), PortErrorClass::Timeout));

    assert_eq!(
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap(),
        STORAGE_SCHEMA_VERSION
    );
    assert_eq!(
        backend.health_probe(deadline()).await.unwrap(),
        StorageHealth::Healthy
    );

    let error = backend
        .migrate(SchemaVersion(STORAGE_SCHEMA_VERSION.0 + 1), deadline())
        .await
        .unwrap_err();
    assert!(matches!(error.class(), PortErrorClass::InvalidInput));

    backend
        .execute(transaction(20_260_903), deadline())
        .await
        .unwrap();
    backend
        .execute(transaction(20_260_903), deadline())
        .await
        .unwrap();
    let error = backend
        .execute(transaction(20_260_904), deadline())
        .await
        .unwrap_err();
    assert!(matches!(error.class(), PortErrorClass::CorruptData));

    backend.checkpoint(deadline()).await.unwrap();
    assert!(!backend.flush(deadline()).await.unwrap().persistence_gap);
    backend.shutdown(deadline()).await.unwrap();
    assert_eq!(
        backend.health_probe(deadline()).await.unwrap(),
        StorageHealth::Stopping
    );
    let error = backend
        .execute(transaction(20_260_905), deadline())
        .await
        .unwrap_err();
    assert!(matches!(error.class(), PortErrorClass::Unavailable));
}

#[tokio::test]
async fn in_memory_backend_follows_storage_port_contract() {
    let backend = InMemoryStorageBackend::new();
    assert_storage_backend_contract(&backend).await;
}

#[tokio::test]
async fn sqlite_backend_follows_storage_port_contract() {
    let path = std::env::temp_dir().join(format!(
        "fluxdns-storage-contract-{}-{}.sqlite3",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let backend = SqliteStorageBackend::connect(&path).await.unwrap();
    assert_storage_backend_contract(&backend).await;

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}
