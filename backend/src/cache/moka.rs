//! 基于 Moka 的并发内存 `CacheStore` adapter。

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use moka::notification::RemovalCause;
use moka::policy::Expiry;
use moka::sync::Cache;
use thiserror::Error;

use super::memory::MemoryCacheStore;
use crate::dns::{Cancellation, Deadline};
use crate::ports::cache::{
    CacheCondition, CacheInvalidation, CacheKey, CacheLoadCompletion, CacheLoadFailure,
    CacheLoadLease, CacheLoadReservation, CacheLoadWaiter, CacheRecord, CacheStore,
    CacheStoreStats, CacheVersion, CacheWriteOutcome,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

#[derive(Clone)]
pub struct MokaCacheStore {
    cache: Cache<CacheKey, CacheRecord>,
    load_store: MemoryCacheStore,
    state: Arc<Mutex<MokaState>>,
    next_version: Arc<AtomicU64>,
    max_weight: Option<u64>,
    eviction_count: Arc<AtomicU64>,
}

#[derive(Default)]
struct MokaState {
    hits: u64,
    misses: u64,
    conflicts: u64,
    shutting_down: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum MokaCacheStoreBuildError {
    #[error("moka cache max weight must be greater than zero")]
    ZeroMaxWeight,
}

struct MokaExpiry;

impl Expiry<CacheKey, CacheRecord> for MokaExpiry {
    fn expire_after_create(
        &self,
        _key: &CacheKey,
        value: &CacheRecord,
        created_at: Instant,
    ) -> Option<Duration> {
        let end = value
            .entry
            .stale_until
            .map_or(value.entry.expires_at, |stale_until| {
                value.entry.expires_at.max(stale_until)
            });
        Some(end.saturating_duration_since(created_at))
    }
}

fn weight(key: &CacheKey, record: &CacheRecord) -> u64 {
    let response_size = record
        .entry
        .response
        .as_message()
        .to_vec()
        .map_or(0, |bytes| bytes.len() as u64);
    (key.encoded.len() as u64)
        .saturating_add(64)
        .saturating_add(response_size)
}

fn moka_weight(key: &CacheKey, record: &CacheRecord) -> u32 {
    weight(key, record).min(u32::MAX as u64) as u32
}

fn is_visible(record: &CacheRecord, now: Instant) -> bool {
    now < record.entry.expires_at
        || record
            .entry
            .stale_until
            .is_some_and(|stale_until| now < stale_until)
}

fn is_fresh(record: &CacheRecord, now: Instant) -> bool {
    now < record.entry.expires_at
}

fn lock<'a, T>(
    mutex: &'a Mutex<T>,
    operation: &'static str,
) -> Result<MutexGuard<'a, T>, PortError> {
    mutex
        .lock()
        .map_err(|_| PortError::new(PortErrorClass::Internal, operation))
}

fn timeout(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Timeout, operation)
}

fn unavailable(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Unavailable, operation)
}

impl Default for MokaCacheStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MokaCacheStore {
    /// 创建不设置容量上限的 Moka store。
    pub fn new() -> Self {
        Self::with_limit(None)
    }

    /// 创建带总计费权重上限的 Moka store。
    pub fn with_max_weight(max_weight: u64) -> Result<Self, MokaCacheStoreBuildError> {
        if max_weight == 0 {
            return Err(MokaCacheStoreBuildError::ZeroMaxWeight);
        }
        Ok(Self::with_limit(Some(max_weight)))
    }

    pub const fn max_weight(&self) -> Option<u64> {
        self.max_weight
    }

    fn with_limit(max_weight: Option<u64>) -> Self {
        let eviction_count = Arc::new(AtomicU64::new(0));
        let eviction_count_for_listener = Arc::clone(&eviction_count);
        let mut builder = Cache::builder()
            .weigher(moka_weight)
            .expire_after(MokaExpiry)
            .eviction_listener(move |_key, _value, cause| {
                if cause == RemovalCause::Size {
                    eviction_count_for_listener.fetch_add(1, Ordering::Relaxed);
                }
            });
        if let Some(max_weight) = max_weight {
            builder = builder.max_capacity(max_weight);
        }
        Self {
            cache: builder.build(),
            load_store: MemoryCacheStore::default(),
            state: Arc::new(Mutex::new(MokaState::default())),
            next_version: Arc::new(AtomicU64::new(0)),
            max_weight,
            eviction_count,
        }
    }

    fn get_record(&self, key: &CacheKey) -> Option<CacheRecord> {
        self.cache.run_pending_tasks();
        self.cache.get(key)
    }

    fn visible_record(&self, key: &CacheKey, now: Instant) -> Option<CacheRecord> {
        let record = self.get_record(key)?;
        if is_visible(&record, now) {
            Some(record)
        } else {
            self.cache.invalidate(key);
            self.cache.run_pending_tasks();
            None
        }
    }

    async fn delegate<F, T>(future: F) -> Result<T, PortError>
    where
        F: Future<Output = Result<T, PortError>>,
    {
        future.await
    }
}

impl fmt::Debug for MokaCacheStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stats = self.stats();
        formatter
            .debug_struct("MokaCacheStore")
            .field("entries", &stats.entries)
            .field("weighted_size", &stats.weighted_size)
            .field("hits", &stats.hits)
            .field("misses", &stats.misses)
            .field("conflicts", &stats.conflicts)
            .field("evictions", &stats.evictions)
            .finish()
    }
}

impl CacheStore for MokaCacheStore {
    fn get<'a>(
        &'a self,
        key: &'a CacheKey,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<Option<CacheRecord>, PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("moka_cache.get"));
            }
            let record = self.visible_record(key, Instant::now());
            let mut state = lock(&self.state, "moka_cache.get")?;
            if state.shutting_down {
                return Err(unavailable("moka_cache.get"));
            }
            if record.is_some() {
                state.hits = state.hits.saturating_add(1);
            } else {
                state.misses = state.misses.saturating_add(1);
            }
            Ok(record)
        })
    }

    fn compare_and_swap<'a>(
        &'a self,
        key: CacheKey,
        condition: CacheCondition,
        entry: Arc<crate::ports::cache::CacheEntry>,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<CacheWriteOutcome, PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("moka_cache.compare_and_swap"));
            }
            let mut state = lock(&self.state, "moka_cache.compare_and_swap")?;
            if state.shutting_down {
                return Err(unavailable("moka_cache.compare_and_swap"));
            }
            let now = Instant::now();
            let current_record = self.visible_record(&key, now);
            let current = current_record.as_ref().map(|record| record.version);
            let condition_matches = match condition {
                CacheCondition::Absent => current.is_none(),
                CacheCondition::Version(expected) => current == Some(expected),
            };
            if !condition_matches {
                state.conflicts = state.conflicts.saturating_add(1);
                return Ok(CacheWriteOutcome::Conflict(current));
            }
            if current_record
                .as_ref()
                .is_some_and(|record| record.entry.quality > entry.quality && is_fresh(record, now))
            {
                return Ok(CacheWriteOutcome::RejectedQuality);
            }
            let candidate = CacheRecord {
                version: CacheVersion(0),
                entry: Arc::clone(&entry),
            };
            if self
                .max_weight
                .is_some_and(|limit| weight(&key, &candidate) > limit)
            {
                return Err(PortError::new(
                    PortErrorClass::ResourceExhausted,
                    "moka_cache.compare_and_swap",
                ));
            }
            let version = CacheVersion(self.next_version.fetch_add(1, Ordering::AcqRel) + 1);
            let outcome = if current.is_some() {
                CacheWriteOutcome::Replaced(version)
            } else {
                CacheWriteOutcome::Inserted(version)
            };
            self.cache.insert(key, CacheRecord { version, entry });
            self.cache.run_pending_tasks();
            Ok(outcome)
        })
    }

    fn reserve_load<'a>(
        &'a self,
        key: CacheKey,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<CacheLoadReservation, PortError>> {
        Box::pin(async move { Self::delegate(self.load_store.reserve_load(key, deadline)).await })
    }

    fn publish_load<'a>(
        &'a self,
        lease: CacheLoadLease,
        completion: CacheLoadCompletion,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            Self::delegate(self.load_store.publish_load(lease, completion, deadline)).await
        })
    }

    fn abandon_load<'a>(
        &'a self,
        lease: CacheLoadLease,
        failure: CacheLoadFailure,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            Self::delegate(self.load_store.abandon_load(lease, failure, deadline)).await
        })
    }

    fn wait_load<'a>(
        &'a self,
        waiter: CacheLoadWaiter,
        deadline: Deadline,
        waiter_cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<CacheLoadCompletion, PortError>> {
        Box::pin(async move {
            Self::delegate(
                self.load_store
                    .wait_load(waiter, deadline, waiter_cancellation),
            )
            .await
        })
    }

    fn invalidate(
        &self,
        scope: CacheInvalidation,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<u64, PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("moka_cache.invalidate"));
            }
            let state = lock(&self.state, "moka_cache.invalidate")?;
            if state.shutting_down {
                return Err(unavailable("moka_cache.invalidate"));
            }
            self.cache.run_pending_tasks();
            let keys = self
                .cache
                .iter()
                .filter_map(|(key, record)| {
                    scope.matches(&key, record.entry.as_ref()).then_some(key)
                })
                .collect::<Vec<_>>();
            for key in &keys {
                self.cache.invalidate(key.as_ref());
            }
            self.cache.run_pending_tasks();
            Ok(keys.len() as u64)
        })
    }

    fn stats(&self) -> CacheStoreStats {
        self.cache.run_pending_tasks();
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        CacheStoreStats {
            entries: self.cache.entry_count(),
            weighted_size: self.cache.weighted_size(),
            hits: state.hits,
            misses: state.misses,
            conflicts: state.conflicts,
            evictions: self.eviction_count.load(Ordering::Relaxed),
        }
    }

    fn shutdown(&self, _deadline: Deadline) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            {
                let mut state = lock(&self.state, "moka_cache.shutdown")?;
                state.shutting_down = true;
            }
            self.cache.invalidate_all();
            self.cache.run_pending_tasks();
            self.load_store
                .shutdown(Deadline::new(Instant::now()))
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use super::*;
    use crate::dns::{CanonicalQuery, CanonicalResponse, DnsMessageId, RuntimeRevision};
    use crate::ports::cache::{CacheEntry, CacheNamespace, CacheQuality, CacheResponseClass};

    fn key(value: &[u8]) -> CacheKey {
        CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(value),
            format_version: 1,
        }
    }

    fn entry(
        expires_in: Duration,
        stale_for: Option<Duration>,
        quality: CacheQuality,
    ) -> CacheEntry {
        let now = Instant::now();
        let name = Name::from_str("example.com.").expect("test name is valid");
        let question = Query::query(name, RecordType::A);
        let mut query_message = Message::new(1, MessageType::Query, OpCode::Query);
        query_message.add_query(question.clone());
        let query = CanonicalQuery::from_message(query_message).expect("test query is valid");
        let mut response_message = Message::new(2, MessageType::Response, OpCode::Query);
        response_message.metadata.response_code = ResponseCode::NoError;
        response_message.add_query(question);
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(2))
                .expect("test response is valid");
        CacheEntry {
            response: Arc::new(response),
            upstream:
                crate::ports::cache::CacheUpstreamProvenance::direct_from_validated_config_id(
                    "test-upstream",
                )
                .unwrap(),
            inserted_at: now,
            expires_at: now + expires_in,
            stale_until: stale_for.map(|duration| now + expires_in + duration),
            response_class: CacheResponseClass::NoError,
            producer_revision: RuntimeRevision(1),
            quality,
            checksum: 1,
            format_version: crate::ports::cache::CACHE_ENTRY_FORMAT_VERSION,
        }
    }

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(2))
    }

    #[tokio::test]
    async fn stores_fresh_and_stale_entries_until_stale_window_ends() {
        let store = MokaCacheStore::with_max_weight(4096).expect("weight is valid");
        let cache_key = key(b"fresh-stale");
        let result = store
            .compare_and_swap(
                cache_key.clone(),
                CacheCondition::Absent,
                Arc::new(entry(
                    Duration::from_millis(10),
                    Some(Duration::from_millis(80)),
                    CacheQuality::Complete,
                )),
                deadline(),
            )
            .await
            .expect("insert succeeds");
        assert!(matches!(result, CacheWriteOutcome::Inserted(_)));
        assert!(store.get(&cache_key, deadline()).await.unwrap().is_some());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(store.get(&cache_key, deadline()).await.unwrap().is_some());
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert!(store.get(&cache_key, deadline()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn quality_cas_and_capacity_match_cache_store_contract() {
        let store = MokaCacheStore::with_max_weight(100).expect("weight is valid");
        let cache_key = key(b"quality");
        let inserted = store
            .compare_and_swap(
                cache_key.clone(),
                CacheCondition::Absent,
                Arc::new(entry(Duration::from_secs(60), None, CacheQuality::Complete)),
                deadline(),
            )
            .await
            .unwrap();
        let version = match inserted {
            CacheWriteOutcome::Inserted(version) => version,
            other => panic!("unexpected outcome: {other:?}"),
        };
        let rejected = store
            .compare_and_swap(
                cache_key.clone(),
                CacheCondition::Version(version),
                Arc::new(entry(Duration::from_secs(60), None, CacheQuality::Negative)),
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(rejected, CacheWriteOutcome::RejectedQuality);
        let oversized = store
            .compare_and_swap(
                key(&[b'x'; 128]),
                CacheCondition::Absent,
                Arc::new(entry(Duration::from_secs(60), None, CacheQuality::Complete)),
                deadline(),
            )
            .await;
        assert!(matches!(oversized, Err(error) if error.class().as_str() == "resource_exhausted"));
    }

    #[tokio::test]
    async fn shutdown_rejects_cache_operations_and_releases_loads() {
        let store = MokaCacheStore::default();
        store.shutdown(deadline()).await.unwrap();
        let error = store
            .get(&key(b"after-shutdown"), deadline())
            .await
            .unwrap_err();
        assert_eq!(error.class().as_str(), "unavailable");
        let error = store
            .reserve_load(key(b"after-shutdown"), deadline())
            .await
            .unwrap_err();
        assert_eq!(error.class().as_str(), "unavailable");
    }
}
