//! 内存与 Moka 生产 adapter 共用的 `CacheStore` 契约测试。

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{Name, RecordType};

use crate::dns::{Cancellation, CanonicalQuery, CanonicalResponse, Deadline, DnsMessageId};
use crate::ports::PortErrorClass;
use crate::ports::cache::{
    CacheCondition, CacheEntry, CacheInvalidation, CacheKey, CacheLoadCompletion,
    CacheLoadReservation, CacheNamespace, CacheQuality, CacheResponseClass, CacheStore,
    CacheWriteOutcome,
};

use super::{MemoryCacheStore, MokaCacheStore};

/// 返回留有充足余量的测试 deadline。
fn deadline() -> Deadline {
    Deadline::new(Instant::now() + Duration::from_secs(5))
}

/// 构造不包含 DNS ID 的稳定 cache key。
fn key() -> CacheKey {
    CacheKey {
        namespace: CacheNamespace::Global,
        encoded: Arc::from(&b"cache-contract/example.test/A"[..]),
        format_version: 1,
    }
}

/// 构造可见且质量稳定的 canonical cache entry。
fn entry() -> Arc<CacheEntry> {
    let now = Instant::now();
    let question = Query::query(Name::from_str("example.test.").unwrap(), RecordType::A);
    let mut query_message = Message::new(9, MessageType::Query, OpCode::Query);
    query_message.add_query(question.clone());
    let query = CanonicalQuery::from_message(query_message).unwrap();
    let mut response_message = Message::new(9, MessageType::Response, OpCode::Query);
    response_message.metadata.response_code = ResponseCode::NoError;
    response_message.add_query(question);
    let response =
        CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(9)).unwrap();

    Arc::new(CacheEntry {
        response: Arc::new(response),
        upstream: crate::ports::cache::CacheUpstreamProvenance::direct_from_validated_config_id(
            "test-upstream",
        )
        .unwrap(),
        inserted_at: now,
        expires_at: now + Duration::from_secs(60),
        stale_until: None,
        response_class: CacheResponseClass::NoError,
        producer_revision: crate::dns::RuntimeRevision(1),
        quality: CacheQuality::Complete,
        checksum: 1,
        format_version: crate::ports::cache::CACHE_ENTRY_FORMAT_VERSION,
    })
}

/// 对任意热路径 cache adapter 执行相同的可观测行为断言。
async fn assert_cache_store_contract(store: &dyn CacheStore) {
    let cache_key = key();
    let expired = Deadline::new(Instant::now() - Duration::from_millis(1));
    let error = store.get(&cache_key, expired).await.unwrap_err();
    assert!(matches!(error.class(), PortErrorClass::Timeout));

    assert!(store.get(&cache_key, deadline()).await.unwrap().is_none());
    let inserted = store
        .compare_and_swap(
            cache_key.clone(),
            CacheCondition::Absent,
            entry(),
            deadline(),
        )
        .await
        .unwrap();
    let CacheWriteOutcome::Inserted(version) = inserted else {
        panic!("空 key 必须产生 Inserted 结果");
    };
    assert_eq!(
        store
            .get(&cache_key, deadline())
            .await
            .unwrap()
            .unwrap()
            .version,
        version
    );
    assert_eq!(
        store
            .compare_and_swap(
                cache_key.clone(),
                CacheCondition::Absent,
                entry(),
                deadline(),
            )
            .await
            .unwrap(),
        CacheWriteOutcome::Conflict(Some(version))
    );

    let leader = match store
        .reserve_load(cache_key.clone(), deadline())
        .await
        .unwrap()
    {
        CacheLoadReservation::Leader(leader) => leader,
        CacheLoadReservation::Follower(_) => panic!("首次 reservation 必须成为 leader"),
    };
    let follower = match store
        .reserve_load(cache_key.clone(), deadline())
        .await
        .unwrap()
    {
        CacheLoadReservation::Follower(follower) => follower,
        CacheLoadReservation::Leader(_) => {
            panic!("相同 key 的第二次 reservation 必须成为 follower")
        }
    };
    store
        .publish_load(leader, CacheLoadCompletion::Miss, deadline())
        .await
        .unwrap();
    let waiter_cancellation = Cancellation::new();
    assert!(matches!(
        store
            .wait_load(follower, deadline(), &waiter_cancellation)
            .await
            .unwrap(),
        CacheLoadCompletion::Miss
    ));

    assert_eq!(
        store
            .invalidate(CacheInvalidation::Exact(cache_key.clone()), deadline())
            .await
            .unwrap(),
        1
    );
    assert!(store.get(&cache_key, deadline()).await.unwrap().is_none());

    store.shutdown(deadline()).await.unwrap();
    let error = store.get(&cache_key, deadline()).await.unwrap_err();
    assert!(matches!(error.class(), PortErrorClass::Unavailable));
    assert_eq!(store.stats().entries, 0);
}

#[tokio::test]
async fn memory_backend_follows_cache_store_contract() {
    assert_cache_store_contract(&MemoryCacheStore::default()).await;
}

#[tokio::test]
async fn moka_backend_follows_cache_store_contract() {
    assert_cache_store_contract(&MokaCacheStore::new()).await;
}
