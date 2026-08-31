//! 基于 HashMap/Mutex 的确定性内存 CacheStore。

use std::collections::HashMap;
use std::fmt;
use std::future::poll_fn;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use tokio::select;

use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::cache::{
    CacheCondition, CacheInvalidation, CacheKey, CacheLoadCompletion, CacheLoadFailure,
    CacheLoadLease, CacheLoadLeaseReleaser, CacheLoadReservation, CacheLoadWaiter,
    CacheLoadWaiterReleaser, CacheRecord, CacheStore, CacheStoreStats, CacheVersion,
    CacheWriteOutcome,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

#[derive(Clone)]
pub struct MemoryCacheStore {
    state: Arc<Mutex<MemoryState>>,
    next_version: Arc<AtomicU64>,
    next_generation: Arc<AtomicU64>,
}

#[derive(Default)]
struct MemoryState {
    records: HashMap<CacheKey, CacheRecord>,
    loads: HashMap<CacheKey, MemoryLoad>,
    stats: CacheStoreStats,
    shutting_down: bool,
}

struct MemoryLoad {
    generation: u64,
    completion: Option<CacheLoadCompletion>,
    followers: u64,
    waiters: Vec<Waker>,
}

impl Default for MemoryCacheStore {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(MemoryState::default())),
            next_version: Arc::new(AtomicU64::new(0)),
            next_generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl fmt::Debug for MemoryCacheStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stats = self.stats();
        formatter
            .debug_struct("MemoryCacheStore")
            .field("entries", &stats.entries)
            .field("weighted_size", &stats.weighted_size)
            .field("hits", &stats.hits)
            .field("misses", &stats.misses)
            .field("conflicts", &stats.conflicts)
            .finish()
    }
}

struct MemoryLeaseReleaser {
    state: Weak<Mutex<MemoryState>>,
}

impl CacheLoadLeaseReleaser for MemoryLeaseReleaser {
    fn abandon(&self, key: &CacheKey, generation: u64) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Ok(waiters) = complete_load(
            state.as_ref(),
            key,
            generation,
            CacheLoadCompletion::Failed(CacheLoadFailure::Abandoned),
        ) else {
            return;
        };
        wake_all(waiters);
    }
}

struct MemoryWaiterReleaser {
    state: Weak<Mutex<MemoryState>>,
}

impl CacheLoadWaiterReleaser for MemoryWaiterReleaser {
    fn release(&self, key: &CacheKey, generation: u64) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        release_waiter(state.as_ref(), key, generation);
    }
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

fn weighted_size(key: &CacheKey, record: &CacheRecord) -> u64 {
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

fn refresh_stats(state: &mut MemoryState) {
    state.stats.entries = state.records.len() as u64;
    state.stats.weighted_size = state
        .records
        .iter()
        .map(|(key, record)| weighted_size(key, record))
        .fold(0, u64::saturating_add);
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

fn complete_load(
    state: &Mutex<MemoryState>,
    key: &CacheKey,
    generation: u64,
    completion: CacheLoadCompletion,
) -> Result<Vec<Waker>, PortError> {
    let mut state = lock(state, "memory_cache.complete_load")?;
    let (waiters, should_remove) = {
        let load = state.loads.get_mut(key).ok_or_else(|| {
            PortError::new(
                PortErrorClass::ProtocolViolation,
                "memory_cache.complete_load",
            )
        })?;
        if load.generation != generation || load.completion.is_some() {
            return Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "memory_cache.complete_load",
            ));
        }
        load.completion = Some(completion);
        (std::mem::take(&mut load.waiters), load.followers == 0)
    };
    if should_remove {
        state.loads.remove(key);
    }
    Ok(waiters)
}

fn release_waiter(state: &Mutex<MemoryState>, key: &CacheKey, generation: u64) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    let should_remove = {
        let Some(load) = state.loads.get_mut(key) else {
            return;
        };
        if load.generation != generation || load.followers == 0 {
            return;
        }
        load.followers -= 1;
        load.completion.is_some() && load.followers == 0
    };
    if should_remove {
        state.loads.remove(key);
    }
}

fn wake_all(waiters: Vec<Waker>) {
    for waiter in waiters {
        waiter.wake();
    }
}

impl MemoryCacheStore {
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn poll_load(
        &self,
        waiter: &CacheLoadWaiter,
        deadline: Deadline,
        context: &mut Context<'_>,
    ) -> Poll<Result<CacheLoadCompletion, PortError>> {
        let mut state = match lock(&self.state, "memory_cache.wait_load") {
            Ok(state) => state,
            Err(error) => return Poll::Ready(Err(error)),
        };
        let Some(load) = state.loads.get_mut(waiter.key()) else {
            return Poll::Ready(Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "memory_cache.wait_load",
            )));
        };
        if load.generation != waiter.generation() {
            return Poll::Ready(Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "memory_cache.wait_load",
            )));
        }
        if let Some(completion) = &load.completion {
            return Poll::Ready(Ok(completion.clone()));
        }
        if deadline.is_expired(Instant::now()) {
            return Poll::Ready(Err(timeout("memory_cache.wait_load")));
        }
        if !load
            .waiters
            .iter()
            .any(|waker| waker.will_wake(context.waker()))
        {
            load.waiters.push(context.waker().clone());
        }
        Poll::Pending
    }
}

impl CacheStore for MemoryCacheStore {
    fn get<'a>(
        &'a self,
        key: &'a CacheKey,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<Option<CacheRecord>, PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("memory_cache.get"));
            }
            let mut state = lock(&self.state, "memory_cache.get")?;
            if state.shutting_down {
                return Err(unavailable("memory_cache.get"));
            }
            let now = Instant::now();
            let visibility = state
                .records
                .get(key)
                .map(|record| (is_visible(record, now), record.clone()));
            match visibility {
                Some((true, record)) => {
                    state.stats.hits = state.stats.hits.saturating_add(1);
                    Ok(Some(record))
                }
                Some((false, _)) => {
                    state.records.remove(key);
                    state.stats.misses = state.stats.misses.saturating_add(1);
                    refresh_stats(&mut state);
                    Ok(None)
                }
                None => {
                    state.stats.misses = state.stats.misses.saturating_add(1);
                    Ok(None)
                }
            }
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
                return Err(timeout("memory_cache.compare_and_swap"));
            }
            let mut state = lock(&self.state, "memory_cache.compare_and_swap")?;
            if state.shutting_down {
                return Err(unavailable("memory_cache.compare_and_swap"));
            }
            let now = Instant::now();
            if state
                .records
                .get(&key)
                .is_some_and(|record| !is_visible(record, now))
            {
                state.records.remove(&key);
                refresh_stats(&mut state);
            }
            let current = state.records.get(&key).map(|record| record.version);
            let condition_matches = match condition {
                CacheCondition::Absent => current.is_none(),
                CacheCondition::Version(expected) => current == Some(expected),
            };
            if !condition_matches {
                state.stats.conflicts = state.stats.conflicts.saturating_add(1);
                return Ok(CacheWriteOutcome::Conflict(current));
            }
            if state
                .records
                .get(&key)
                .is_some_and(|record| record.entry.quality > entry.quality && is_fresh(record, now))
            {
                return Ok(CacheWriteOutcome::RejectedQuality);
            }

            let version = CacheVersion(self.next_version.fetch_add(1, Ordering::AcqRel) + 1);
            let outcome = if current.is_some() {
                CacheWriteOutcome::Replaced(version)
            } else {
                CacheWriteOutcome::Inserted(version)
            };
            state.records.insert(key, CacheRecord { version, entry });
            refresh_stats(&mut state);
            Ok(outcome)
        })
    }

    fn reserve_load<'a>(
        &'a self,
        key: CacheKey,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<CacheLoadReservation, PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("memory_cache.reserve_load"));
            }
            let mut state = lock(&self.state, "memory_cache.reserve_load")?;
            if state.shutting_down {
                return Err(unavailable("memory_cache.reserve_load"));
            }
            if let Some(load) = state.loads.get_mut(&key) {
                load.followers = load.followers.saturating_add(1);
                return Ok(CacheLoadReservation::Follower(CacheLoadWaiter::new(
                    key,
                    load.generation,
                    Arc::new(MemoryWaiterReleaser {
                        state: Arc::downgrade(&self.state),
                    }),
                )));
            }
            let generation = self.next_generation.fetch_add(1, Ordering::AcqRel) + 1;
            state.loads.insert(
                key.clone(),
                MemoryLoad {
                    generation,
                    completion: None,
                    followers: 0,
                    waiters: Vec::new(),
                },
            );
            Ok(CacheLoadReservation::Leader(CacheLoadLease::new(
                key,
                generation,
                Arc::new(MemoryLeaseReleaser {
                    state: Arc::downgrade(&self.state),
                }),
            )))
        })
    }

    fn publish_load<'a>(
        &'a self,
        lease: CacheLoadLease,
        completion: CacheLoadCompletion,
        _deadline: Deadline,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let waiters = complete_load(
                self.state.as_ref(),
                lease.key(),
                lease.generation(),
                completion,
            )?;
            lease.disarm();
            wake_all(waiters);
            Ok(())
        })
    }

    fn abandon_load<'a>(
        &'a self,
        lease: CacheLoadLease,
        failure: CacheLoadFailure,
        _deadline: Deadline,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let waiters = complete_load(
                self.state.as_ref(),
                lease.key(),
                lease.generation(),
                CacheLoadCompletion::Failed(failure),
            )?;
            lease.disarm();
            wake_all(waiters);
            Ok(())
        })
    }

    fn wait_load<'a>(
        &'a self,
        waiter: CacheLoadWaiter,
        deadline: Deadline,
        waiter_cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<CacheLoadCompletion, PortError>> {
        Box::pin(async move {
            select! {
                completion = poll_fn(|context| self.poll_load(&waiter, deadline, context)) => completion,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.at())) => Err(timeout("memory_cache.wait_load")),
                _ = waiter_cancellation.cancelled() => Err(PortError::new(
                    PortErrorClass::Cancelled(
                        waiter_cancellation.reason().unwrap_or(CancelReason::DeadlineExceeded),
                    ),
                    "memory_cache.wait_load",
                )),
            }
        })
    }

    fn invalidate(
        &self,
        scope: CacheInvalidation,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<u64, PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("memory_cache.invalidate"));
            }
            let mut state = lock(&self.state, "memory_cache.invalidate")?;
            if state.shutting_down {
                return Err(unavailable("memory_cache.invalidate"));
            }
            let before = state.records.len();
            state
                .records
                .retain(|key, record| !scope.matches(key, record.entry.as_ref()));
            let removed = before.saturating_sub(state.records.len()) as u64;
            refresh_stats(&mut state);
            Ok(removed)
        })
    }

    fn stats(&self) -> CacheStoreStats {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stats
    }

    fn shutdown(&self, _deadline: Deadline) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let waiters = {
                let mut state = lock(&self.state, "memory_cache.shutdown")?;
                state.shutting_down = true;
                state.records.clear();
                refresh_stats(&mut state);
                let mut waiters = Vec::new();
                for load in state.loads.values_mut() {
                    if load.completion.is_none() {
                        load.completion = Some(CacheLoadCompletion::Failed(
                            CacheLoadFailure::Cancelled(CancelReason::Shutdown),
                        ));
                    }
                    waiters.extend(std::mem::take(&mut load.waiters));
                }
                state.loads.retain(|_, load| load.followers > 0);
                waiters
            };
            wake_all(waiters);
            Ok(())
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

    use crate::dns::{
        CancelReason, Cancellation, CanonicalQuery, CanonicalResponse, Deadline, DnsMessageId,
        RuntimeRevision,
    };
    use crate::ports::PortErrorClass;
    use crate::ports::PortErrorClass;
    use crate::ports::cache::{
        CacheCondition, CacheInvalidation, CacheInvalidationPredicate, CacheLoadCompletion,
        CacheLoadFailure, CacheLoadReservation, CacheNamespace, CacheQuality, CacheResponseClass,
        CacheStore, CacheVersion,
    };

    use super::MemoryCacheStore;

    fn deadline() -> Deadline {
        Deadline::new(std::time::Instant::now() + Duration::from_secs(30))
    }

    fn key(name: &str) -> crate::ports::cache::CacheKey {
        crate::ports::cache::CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(name.as_bytes()),
            format_version: 1,
        }
    }

    fn response() -> CanonicalResponse {
        let mut query_message = Message::new(1, MessageType::Query, OpCode::Query);
        query_message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        let query = CanonicalQuery::from_message(query_message).unwrap();
        let mut response_message = Message::new(2, MessageType::Response, OpCode::Query);
        response_message.metadata.response_code = ResponseCode::NoError;
        response_message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(2)).unwrap()
    }

    fn record(
        revision: u64,
        quality: CacheQuality,
        expires_at: std::time::Instant,
        stale_until: Option<std::time::Instant>,
    ) -> crate::ports::cache::CacheRecord {
        crate::ports::cache::CacheRecord {
            version: CacheVersion(revision),
            entry: Arc::new(crate::ports::cache::CacheEntry {
                response: Arc::new(response()),
                inserted_at: std::time::Instant::now(),
                expires_at,
                stale_until,
                response_class: CacheResponseClass::NoError,
                producer_revision: RuntimeRevision(revision),
                quality,
                checksum: revision,
                format_version: 1,
            }),
        }
    }

    #[tokio::test]
    async fn get_handles_fresh_stale_and_expired_entries() {
        let store = MemoryCacheStore::default();
        let now = std::time::Instant::now();
        store
            .compare_and_swap(
                key("fresh"),
                CacheCondition::Absent,
                record(
                    1,
                    CacheQuality::Complete,
                    now + Duration::from_secs(30),
                    None,
                )
                .entry,
                deadline(),
            )
            .await
            .unwrap();
        store
            .compare_and_swap(
                key("stale"),
                CacheCondition::Absent,
                record(
                    2,
                    CacheQuality::Negative,
                    now - Duration::from_secs(1),
                    Some(now + Duration::from_secs(30)),
                )
                .entry,
                deadline(),
            )
            .await
            .unwrap();
        store
            .compare_and_swap(
                key("expired"),
                CacheCondition::Absent,
                record(3, CacheQuality::Failure, now - Duration::from_secs(1), None).entry,
                deadline(),
            )
            .await
            .unwrap();

        assert!(
            store
                .get(&key("fresh"), deadline())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get(&key("stale"), deadline())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get(&key("expired"), deadline())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(store.stats().hits, 2);
        assert_eq!(store.stats().misses, 1);
        assert_eq!(store.len(), 2);
    }

    #[tokio::test]
    async fn cas_checks_condition_and_rejects_lower_quality_fresh_entry() {
        let store = MemoryCacheStore::default();
        let cache_key = key("quality");
        let inserted = store
            .compare_and_swap(
                cache_key.clone(),
                CacheCondition::Absent,
                record(
                    1,
                    CacheQuality::Complete,
                    std::time::Instant::now() + Duration::from_secs(30),
                    None,
                )
                .entry,
                deadline(),
            )
            .await
            .unwrap();
        let version = match inserted {
            crate::ports::cache::CacheWriteOutcome::Inserted(version) => version,
            other => panic!("unexpected insert result: {other:?}"),
        };
        assert_eq!(
            store
                .compare_and_swap(
                    cache_key.clone(),
                    CacheCondition::Version(version),
                    record(
                        2,
                        CacheQuality::Failure,
                        std::time::Instant::now() + Duration::from_secs(30),
                        None,
                    )
                    .entry,
                    deadline(),
                )
                .await
                .unwrap(),
            crate::ports::cache::CacheWriteOutcome::RejectedQuality
        );
        assert!(matches!(
            store
                .compare_and_swap(
                    cache_key,
                    CacheCondition::Version(CacheVersion(999)),
                    record(
                        3,
                        CacheQuality::Complete,
                        std::time::Instant::now() + Duration::from_secs(30),
                        None,
                    )
                    .entry,
                    deadline(),
                )
                .await
                .unwrap(),
            crate::ports::cache::CacheWriteOutcome::Conflict(Some(_))
        ));
        assert_eq!(store.stats().conflicts, 1);
    }

    #[tokio::test]
    async fn invalidation_supports_exact_predicate_namespace_and_all() {
        let store = MemoryCacheStore::default();
        for (name, revision) in [("one", 1), ("two", 2), ("three", 1)] {
            store
                .compare_and_swap(
                    key(name),
                    CacheCondition::Absent,
                    record(
                        revision,
                        CacheQuality::Complete,
                        std::time::Instant::now() + Duration::from_secs(30),
                        None,
                    )
                    .entry,
                    deadline(),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            store
                .invalidate(CacheInvalidation::Exact(key("one")), deadline())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .invalidate(
                    CacheInvalidation::Predicate(CacheInvalidationPredicate::ProducerRevision {
                        namespace: CacheNamespace::Global,
                        revision: RuntimeRevision(1),
                    }),
                    deadline(),
                )
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .invalidate(
                    CacheInvalidation::Namespace(CacheNamespace::Global),
                    deadline()
                )
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .invalidate(CacheInvalidation::All, deadline())
                .await
                .unwrap(),
            0
        );
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn single_flight_publishes_to_followers_and_releases_after_completion() {
        let store = Arc::new(MemoryCacheStore::default());
        let cache_key = key("single-flight");
        let leader = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Leader(lease) => lease,
            CacheLoadReservation::Follower(_) => panic!("expected leader"),
        };
        let waiter = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("expected follower"),
        };
        let waiting_store = Arc::clone(&store);
        let cancellation = Cancellation::new();
        let waiting = tokio::spawn(async move {
            waiting_store
                .wait_load(waiter, deadline(), &cancellation)
                .await
        });
        tokio::task::yield_now().await;
        store
            .publish_load(leader, CacheLoadCompletion::Miss, deadline())
            .await
            .unwrap();
        assert!(matches!(
            waiting.await.unwrap().unwrap(),
            CacheLoadCompletion::Miss
        ));
        assert!(matches!(
            store.reserve_load(cache_key, deadline()).await.unwrap(),
            CacheLoadReservation::Leader(_)
        ));
    }

    #[tokio::test]
    async fn waiter_cancellation_is_independent_and_leader_abandon_wakes_followers() {
        let store = Arc::new(MemoryCacheStore::default());
        let cache_key = key("cancel-abandon");
        let leader = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Leader(lease) => lease,
            CacheLoadReservation::Follower(_) => panic!("expected leader"),
        };
        let cancelled_waiter = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("expected follower"),
        };
        let remaining_waiter = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("expected follower"),
        };
        let cancelled = Cancellation::new();
        let cancelled_task = tokio::spawn({
            let store = Arc::clone(&store);
            let cancelled = cancelled.clone();
            async move {
                store
                    .wait_load(cancelled_waiter, deadline(), &cancelled)
                    .await
            }
        });
        tokio::task::yield_now().await;
        cancelled.cancel(CancelReason::ClientDisconnected);
        assert!(cancelled_task.await.unwrap().is_err());

        let waiting = tokio::spawn({
            let store = Arc::clone(&store);
            let cancellation = Cancellation::new();
            async move {
                store
                    .wait_load(remaining_waiter, deadline(), &cancellation)
                    .await
            }
        });
        tokio::task::yield_now().await;
        store
            .abandon_load(leader, CacheLoadFailure::Unavailable, deadline())
            .await
            .unwrap();
        assert!(matches!(
            waiting.await.unwrap().unwrap(),
            CacheLoadCompletion::Failed(CacheLoadFailure::Unavailable)
        ));
        assert!(matches!(
            store.reserve_load(cache_key, deadline()).await.unwrap(),
            CacheLoadReservation::Leader(_)
        ));
    }

    #[tokio::test]
    async fn waiter_deadline_returns_timeout_without_cancelling_leader() {
        let store = Arc::new(MemoryCacheStore::default());
        let cache_key = key("waiter-deadline");
        let leader = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Leader(lease) => lease,
            CacheLoadReservation::Follower(_) => panic!("expected leader"),
        };
        let waiter = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("expected follower"),
        };
        let cancellation = Cancellation::new();
        let error = store
            .wait_load(
                waiter,
                Deadline::new(std::time::Instant::now() + Duration::from_millis(5)),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::Timeout));

        store
            .abandon_load(leader, CacheLoadFailure::Abandoned, deadline())
            .await
            .unwrap();
        assert!(matches!(
            store.reserve_load(cache_key, deadline()).await.unwrap(),
            CacheLoadReservation::Leader(_)
        ));
    }

    #[tokio::test]
    async fn shutdown_wakes_waiters_and_rejects_new_operations() {
        let store = Arc::new(MemoryCacheStore::default());
        let cache_key = key("shutdown");
        let leader = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Leader(lease) => lease,
            CacheLoadReservation::Follower(_) => panic!("expected leader"),
        };
        let waiter = match store
            .reserve_load(cache_key.clone(), deadline())
            .await
            .unwrap()
        {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("expected follower"),
        };
        let waiting = tokio::spawn({
            let store = Arc::clone(&store);
            let cancellation = Cancellation::new();
            async move { store.wait_load(waiter, deadline(), &cancellation).await }
        });

        tokio::task::yield_now().await;
        store.shutdown(deadline()).await.unwrap();
        assert!(matches!(
            waiting.await.unwrap().unwrap(),
            CacheLoadCompletion::Failed(CacheLoadFailure::Cancelled(CancelReason::Shutdown))
        ));
        assert!(matches!(
            store.get(&cache_key, deadline()).await.unwrap_err().class(),
            PortErrorClass::Unavailable
        ));
        assert!(matches!(
            store
                .reserve_load(cache_key, deadline())
                .await
                .unwrap_err()
                .class(),
            PortErrorClass::Unavailable
        ));
        drop(leader);
    }
}
