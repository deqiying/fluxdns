//! Adapter contract test 可复用的确定性 fake。

use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant, SystemTime};

use crate::dns::{CancelReason, Cancellation, CanonicalQuery, Deadline, RequestContext};

use super::cache::{
    CacheCondition, CacheInvalidation, CacheKey, CacheLoadCompletion, CacheLoadFailure,
    CacheLoadLease, CacheLoadLeaseReleaser, CacheLoadReservation, CacheLoadWaiter,
    CacheLoadWaiterReleaser, CacheRecord, CacheStore, CacheStoreStats, CacheVersion,
    CacheWriteOutcome,
};
use super::effects::Clock;
use super::exchange::{
    ConnectorId, DnsExchange, TransportFailure, TransportFailureClass, UpstreamOutcome,
};
use super::inbound::{InboundAdapter, InboundRequest, ResponseEncoder};
use super::telemetry::{
    LogEvent, LogSink, MetricEvent, MetricLabelKey, MetricsSink, TelemetryFieldError,
    TelemetryFlushSummary,
};
use super::{PortError, PortErrorClass, PortFuture};

fn lock<'a, T>(
    mutex: &'a Mutex<T>,
    operation: &'static str,
) -> Result<MutexGuard<'a, T>, PortError> {
    mutex
        .lock()
        .map_err(|_| PortError::new(PortErrorClass::Internal, operation))
}

struct FakeClockTimes {
    monotonic: Instant,
    utc: SystemTime,
}

struct FakeClockState {
    times: Mutex<FakeClockTimes>,
    waiters: Mutex<Vec<Waker>>,
}

/// 手动推进的确定性时钟；推进会唤醒所有等待者重新检查 deadline。
#[derive(Clone)]
pub struct FakeClock {
    state: Arc<FakeClockState>,
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new(Instant::now(), SystemTime::now())
    }
}

impl FakeClock {
    pub fn new(monotonic: Instant, utc: SystemTime) -> Self {
        Self {
            state: Arc::new(FakeClockState {
                times: Mutex::new(FakeClockTimes { monotonic, utc }),
                waiters: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn advance(&self, duration: Duration) {
        let mut times = self
            .state
            .times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(monotonic) = times.monotonic.checked_add(duration) {
            times.monotonic = monotonic;
        }
        if let Some(utc) = times.utc.checked_add(duration) {
            times.utc = utc;
        }
        drop(times);

        let waiters = {
            let mut waiters = self
                .state
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

impl Clock for FakeClock {
    fn monotonic_now(&self) -> Instant {
        self.state
            .times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .monotonic
    }

    fn utc_now(&self) -> SystemTime {
        self.state
            .times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .utc
    }

    fn sleep_until(&self, deadline: Deadline) -> PortFuture<'_, ()> {
        Box::pin(poll_fn(move |context| {
            if deadline.is_expired(self.monotonic_now()) {
                return Poll::Ready(());
            }

            let mut waiters = self
                .state
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if deadline.is_expired(self.monotonic_now()) {
                return Poll::Ready(());
            }
            if !waiters.iter().any(|waker| waker.will_wake(context.waker())) {
                waiters.push(context.waker().clone());
            }
            Poll::Pending
        }))
    }
}

/// 由测试显式排队 response/failure/cancelled 结果的 exchange。
pub struct FakeExchange {
    connector: ConnectorId,
    outcomes: Mutex<VecDeque<UpstreamOutcome>>,
    calls: AtomicU64,
}

impl FakeExchange {
    pub fn new(connector: ConnectorId) -> Self {
        Self {
            connector,
            outcomes: Mutex::new(VecDeque::new()),
            calls: AtomicU64::new(0),
        }
    }

    pub fn push(&self, outcome: UpstreamOutcome) -> Result<(), PortError> {
        lock(&self.outcomes, "fake_exchange.push")?.push_back(outcome);
        Ok(())
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Acquire)
    }
}

impl DnsExchange for FakeExchange {
    fn connector_id(&self) -> &ConnectorId {
        &self.connector
    }

    fn exchange<'a>(
        &'a self,
        _query: &'a CanonicalQuery,
        _context: &'a RequestContext,
    ) -> PortFuture<'a, UpstreamOutcome> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let outcome = lock(&self.outcomes, "fake_exchange.exchange")
            .ok()
            .and_then(|mut outcomes| outcomes.pop_front())
            .unwrap_or_else(|| {
                UpstreamOutcome::TransportFailure(TransportFailure {
                    connector: self.connector.clone(),
                    class: TransportFailureClass::Unavailable,
                    retryable: true,
                    safe_context: Some("fake queue empty"),
                })
            });
        Box::pin(async move { outcome })
    }
}

/// 确定性入站 fake；队列耗尽后返回 `None`。
#[derive(Default)]
pub struct FakeInboundAdapter {
    requests: Mutex<VecDeque<InboundRequest>>,
    closed: AtomicBool,
}

impl FakeInboundAdapter {
    pub fn push(&self, request: InboundRequest) -> Result<(), PortError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PortError::new(
                PortErrorClass::Unavailable,
                "fake_inbound.push",
            ));
        }
        lock(&self.requests, "fake_inbound.push")?.push_back(request);
        Ok(())
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl InboundAdapter for FakeInboundAdapter {
    fn receive<'a>(
        &'a self,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<InboundRequest>, PortError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(PortError::new(
                    PortErrorClass::Cancelled(
                        cancellation.reason().unwrap_or(CancelReason::Shutdown),
                    ),
                    "fake_inbound.receive",
                ));
            }
            let request = lock(&self.requests, "fake_inbound.receive")?.pop_front();
            Ok(request)
        })
    }
}

/// 记录实际进入 encoder 的 response，用于验证 correlation exactly-once。
#[derive(Default)]
pub struct FakeResponseEncoder {
    responses: Mutex<Vec<Arc<crate::dns::CanonicalResponse>>>,
}

impl FakeResponseEncoder {
    pub fn encoded_count(&self) -> usize {
        self.responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl ResponseEncoder for FakeResponseEncoder {
    fn encode<'a>(
        &'a self,
        _request: &'a crate::dns::DnsRequest,
        response: Arc<crate::dns::CanonicalResponse>,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            lock(&self.responses, "fake_response_encoder.encode")?.push(response);
            Ok(())
        })
    }
}

#[derive(Default)]
struct FakeCacheState {
    records: HashMap<CacheKey, CacheRecord>,
    loads: HashMap<CacheKey, FakeCacheLoad>,
    stats: CacheStoreStats,
}

struct FakeCacheLoad {
    generation: u64,
    completion: Option<CacheLoadCompletion>,
    followers: u64,
    waiters: Vec<Waker>,
}

struct FakeCacheLeaseReleaser {
    state: Weak<Mutex<FakeCacheState>>,
}

impl CacheLoadLeaseReleaser for FakeCacheLeaseReleaser {
    fn abandon(&self, key: &CacheKey, generation: u64) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Ok(waiters) = complete_fake_load(
            state.as_ref(),
            key,
            generation,
            CacheLoadCompletion::Failed(CacheLoadFailure::Abandoned),
        ) else {
            return;
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

struct FakeCacheWaiterReleaser {
    state: Weak<Mutex<FakeCacheState>>,
}

impl CacheLoadWaiterReleaser for FakeCacheWaiterReleaser {
    fn release(&self, key: &CacheKey, generation: u64) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        release_fake_waiter(state.as_ref(), key, generation);
    }
}

fn complete_fake_load(
    state: &Mutex<FakeCacheState>,
    key: &CacheKey,
    generation: u64,
    completion: CacheLoadCompletion,
) -> Result<Vec<Waker>, PortError> {
    let mut state = lock(state, "fake_cache.complete_load")?;
    let (waiters, should_remove) = {
        let load = state.loads.get_mut(key).ok_or_else(|| {
            PortError::new(
                PortErrorClass::ProtocolViolation,
                "fake_cache.complete_load",
            )
        })?;
        if load.generation != generation || load.completion.is_some() {
            return Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "fake_cache.complete_load",
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

fn release_fake_waiter(state: &Mutex<FakeCacheState>, key: &CacheKey, generation: u64) {
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

/// 以 mutex 保证原子 CAS 的内存 fake。
#[derive(Default)]
pub struct FakeCacheStore {
    state: Arc<Mutex<FakeCacheState>>,
    next_version: AtomicU64,
    next_load_generation: AtomicU64,
}

impl FakeCacheStore {
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
        context: &mut std::task::Context<'_>,
    ) -> Poll<Result<CacheLoadCompletion, PortError>> {
        let mut state = match lock(&self.state, "fake_cache.wait_load") {
            Ok(state) => state,
            Err(error) => return Poll::Ready(Err(error)),
        };
        let Some(load) = state.loads.get_mut(waiter.key()) else {
            return Poll::Ready(Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "fake_cache.wait_load",
            )));
        };
        if load.generation != waiter.generation() {
            return Poll::Ready(Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "fake_cache.wait_load",
            )));
        }
        if let Some(completion) = &load.completion {
            return Poll::Ready(Ok(completion.clone()));
        }
        if deadline.is_expired(Instant::now()) {
            return Poll::Ready(Err(PortError::new(
                PortErrorClass::Timeout,
                "fake_cache.wait_load",
            )));
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

impl CacheStore for FakeCacheStore {
    fn get<'a>(
        &'a self,
        key: &'a CacheKey,
        _deadline: Deadline,
    ) -> PortFuture<'a, Result<Option<CacheRecord>, PortError>> {
        Box::pin(async move {
            let mut state = lock(&self.state, "fake_cache.get")?;
            let record = state.records.get(key).cloned();
            if record.is_some() {
                state.stats.hits += 1;
            } else {
                state.stats.misses += 1;
            }
            Ok(record)
        })
    }

    fn compare_and_swap<'a>(
        &'a self,
        key: CacheKey,
        condition: CacheCondition,
        entry: Arc<super::cache::CacheEntry>,
        _deadline: Deadline,
    ) -> PortFuture<'a, Result<CacheWriteOutcome, PortError>> {
        Box::pin(async move {
            let mut state = lock(&self.state, "fake_cache.compare_and_swap")?;
            let current = state.records.get(&key).map(|record| record.version);
            let condition_matches = match condition {
                CacheCondition::Absent => current.is_none(),
                CacheCondition::Version(expected) => current == Some(expected),
            };
            if !condition_matches {
                state.stats.conflicts += 1;
                return Ok(CacheWriteOutcome::Conflict(current));
            }

            let version = CacheVersion(self.next_version.fetch_add(1, Ordering::AcqRel) + 1);
            let outcome = if current.is_some() {
                CacheWriteOutcome::Replaced(version)
            } else {
                CacheWriteOutcome::Inserted(version)
            };
            state.records.insert(key, CacheRecord { version, entry });
            state.stats.entries = state.records.len() as u64;
            Ok(outcome)
        })
    }

    fn reserve_load<'a>(
        &'a self,
        key: CacheKey,
        _deadline: Deadline,
    ) -> PortFuture<'a, Result<CacheLoadReservation, PortError>> {
        Box::pin(async move {
            let mut state = lock(&self.state, "fake_cache.reserve_load")?;
            if let Some(load) = state.loads.get_mut(&key) {
                load.followers += 1;
                let generation = load.generation;
                return Ok(CacheLoadReservation::Follower(CacheLoadWaiter::new(
                    key,
                    generation,
                    Arc::new(FakeCacheWaiterReleaser {
                        state: Arc::downgrade(&self.state),
                    }),
                )));
            }

            let generation = self.next_load_generation.fetch_add(1, Ordering::AcqRel) + 1;
            state.loads.insert(
                key.clone(),
                FakeCacheLoad {
                    generation,
                    completion: None,
                    followers: 0,
                    waiters: Vec::new(),
                },
            );
            Ok(CacheLoadReservation::Leader(CacheLoadLease::new(
                key,
                generation,
                Arc::new(FakeCacheLeaseReleaser {
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
            let waiters = complete_fake_load(
                self.state.as_ref(),
                lease.key(),
                lease.generation(),
                completion,
            )?;
            lease.disarm();
            for waiter in waiters {
                waiter.wake();
            }
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
            let waiters = complete_fake_load(
                self.state.as_ref(),
                lease.key(),
                lease.generation(),
                CacheLoadCompletion::Failed(failure),
            )?;
            lease.disarm();
            for waiter in waiters {
                waiter.wake();
            }
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
            let completion = tokio::select! {
                completion = poll_fn(|context| self.poll_load(&waiter, deadline, context)) => completion,
                _ = waiter_cancellation.cancelled() => Err(PortError::new(
                    PortErrorClass::Cancelled(
                        waiter_cancellation.reason().unwrap_or(crate::dns::CancelReason::DeadlineExceeded),
                    ),
                    "fake_cache.wait_load",
                )),
            };
            completion
        })
    }

    fn invalidate(
        &self,
        scope: CacheInvalidation,
        _deadline: Deadline,
    ) -> PortFuture<'_, Result<u64, PortError>> {
        Box::pin(async move {
            let mut state = lock(&self.state, "fake_cache.invalidate")?;
            let before = state.records.len();
            state
                .records
                .retain(|key, record| !scope.matches(key, record.entry.as_ref()));
            let removed = before.saturating_sub(state.records.len()) as u64;
            state.stats.entries = state.records.len() as u64;
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
        Box::pin(async { Ok(()) })
    }
}

/// 捕获 typed 事件，并提供 raw label contract 的拒绝检查。
#[derive(Default)]
pub struct FakeTelemetry {
    metrics: Mutex<Vec<MetricEvent>>,
    logs: Mutex<Vec<LogEvent>>,
}

impl FakeTelemetry {
    pub fn validate_raw_label(key: &str) -> Result<MetricLabelKey, TelemetryFieldError> {
        MetricLabelKey::parse(key)
    }

    pub fn metric_count(&self) -> usize {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl MetricsSink for FakeTelemetry {
    fn record(&self, event: MetricEvent) -> Result<(), PortError> {
        event.validate().map_err(|_| {
            PortError::new(
                PortErrorClass::InvalidInput,
                "fake_telemetry.metric.validate",
            )
        })?;
        lock(&self.metrics, "fake_telemetry.metric")?.push(event);
        Ok(())
    }
}

impl LogSink for FakeTelemetry {
    fn emit(&self, event: LogEvent) -> Result<(), PortError> {
        lock(&self.logs, "fake_telemetry.log")?.push(event);
        Ok(())
    }

    fn flush(
        &self,
        _deadline: Deadline,
    ) -> PortFuture<'_, Result<TelemetryFlushSummary, PortError>> {
        Box::pin(async move {
            let emitted = lock(&self.logs, "fake_telemetry.flush")?.len() as u64;
            Ok(TelemetryFlushSummary {
                emitted,
                ..TelemetryFlushSummary::default()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use crate::dns::{
        CacheCompatibilityKey, CancelReason, Cancellation, CanonicalResponse, ClientIdentity,
        DnsMessageId, DnsRequest, ListenerId, RequestId, RequestMeta, RuntimeRevision,
        TransportCapabilities, TransportClass,
    };

    use super::*;
    use crate::ports::cache::{
        CacheEntry, CacheLoadCompletion, CacheLoadFailure, CacheLoadReservation, CacheNamespace,
        CacheQuality, CacheResponseClass, CacheStore,
    };
    use crate::ports::effects::Clock;
    use crate::ports::exchange::DnsExchange;

    fn fixtures() -> (CanonicalQuery, CanonicalResponse, RequestContext, Deadline) {
        let mut query_message = Message::new(42, MessageType::Query, OpCode::Query);
        query_message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        let query = CanonicalQuery::from_message(query_message).unwrap();

        let mut response_message = Message::new(99, MessageType::Response, OpCode::Query);
        response_message.metadata.response_code = ResponseCode::NXDomain;
        response_message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(99))
                .unwrap();

        let deadline = Deadline::new(Instant::now() + Duration::from_secs(30));
        let context = RequestContext {
            meta: RequestMeta {
                request_id: RequestId(1),
                trace_id: None,
                received_at: Instant::now(),
                received_at_utc: SystemTime::now(),
                deadline,
                cancellation: Cancellation::new(),
                connection_id: None,
                stream_id: None,
                listener_id: ListenerId::from("test"),
                route_id: None,
                original_dns_id: Some(42),
            },
            client: ClientIdentity {
                peer_addr: Some(SocketAddr::from(([127, 0, 0, 1], 5300))),
                ..ClientIdentity::default()
            },
            transport: TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            runtime_revision: RuntimeRevision(7),
        };
        (query, response, context, deadline)
    }

    #[tokio::test]
    async fn fake_inbound_preserves_canonical_request_and_context() {
        let (query, _, context, _) = fixtures();
        let expected = DnsRequest { query, context };
        let encoder = Arc::new(FakeResponseEncoder::default());
        let inbound = FakeInboundAdapter::default();
        inbound
            .push(InboundRequest::new(expected.clone(), encoder))
            .unwrap();

        let receive_cancellation = Cancellation::new();
        let received = inbound
            .receive(&receive_cancellation)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(received.request().query, expected.query);
        assert_eq!(
            received.request().context.meta.request_id,
            expected.context.meta.request_id
        );
        assert_eq!(
            received.request().context.transport,
            expected.context.transport
        );
        assert_eq!(
            received.request().context.runtime_revision,
            expected.context.runtime_revision
        );
    }

    #[tokio::test]
    async fn fake_inbound_observes_accept_loop_cancellation() {
        let inbound = FakeInboundAdapter::default();
        let cancellation = Cancellation::new();
        cancellation.cancel(CancelReason::Shutdown);

        let error = inbound.receive(&cancellation).await.unwrap_err();
        assert!(matches!(
            error.class(),
            PortErrorClass::Cancelled(CancelReason::Shutdown)
        ));
    }

    #[tokio::test]
    async fn fake_clock_advance_wakes_sleep_until() {
        let start = Instant::now();
        let utc = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let clock = FakeClock::new(start, utc);
        let sleeper_clock = clock.clone();
        let sleeper = tokio::spawn(async move {
            sleeper_clock
                .sleep_until(Deadline::new(start + Duration::from_micros(10)))
                .await;
        });
        tokio::task::yield_now().await;

        assert!(!sleeper.is_finished());
        clock.advance(Duration::from_micros(10));
        sleeper.await.unwrap();
        assert_eq!(clock.monotonic_now(), start + Duration::from_micros(10));
        assert_eq!(clock.utc_now(), utc + Duration::from_micros(10));
    }

    #[tokio::test]
    async fn fake_exchange_covers_response_failure_and_cancelled() {
        let (query, response, context, _) = fixtures();
        let connector = ConnectorId::new("primary").unwrap();
        let exchange = FakeExchange::new(connector.clone());
        exchange.push(UpstreamOutcome::Response(response)).unwrap();
        exchange
            .push(UpstreamOutcome::TransportFailure(TransportFailure {
                connector: connector.clone(),
                class: TransportFailureClass::Timeout,
                retryable: true,
                safe_context: None,
            }))
            .unwrap();
        exchange
            .push(UpstreamOutcome::Cancelled(CancelReason::Shutdown))
            .unwrap();

        assert!(matches!(
            exchange.exchange(&query, &context).await,
            UpstreamOutcome::Response(_)
        ));
        assert!(matches!(
            exchange.exchange(&query, &context).await,
            UpstreamOutcome::TransportFailure(TransportFailure {
                class: TransportFailureClass::Timeout,
                ..
            })
        ));
        assert!(matches!(
            exchange.exchange(&query, &context).await,
            UpstreamOutcome::Cancelled(CancelReason::Shutdown)
        ));
        assert_eq!(exchange.calls(), 3);
    }

    #[tokio::test]
    async fn fake_cache_reports_cas_conflicts() {
        let (_, response, _, deadline) = fixtures();
        let store = FakeCacheStore::default();
        let key = CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(&b"example.com/A"[..]),
            format_version: 1,
        };
        let now = Instant::now();
        let entry = Arc::new(CacheEntry {
            response: Arc::new(response),
            upstream:
                crate::ports::cache::CacheUpstreamProvenance::direct_from_validated_config_id(
                    "test-upstream",
                )
                .unwrap(),
            inserted_at: now,
            expires_at: now + Duration::from_secs(60),
            stale_until: None,
            response_class: CacheResponseClass::NxDomain,
            producer_revision: RuntimeRevision(7),
            quality: CacheQuality::Negative,
            checksum: 1,
            format_version: crate::ports::cache::CACHE_ENTRY_FORMAT_VERSION,
        });

        let inserted = store
            .compare_and_swap(key.clone(), CacheCondition::Absent, entry.clone(), deadline)
            .await
            .unwrap();
        assert_eq!(inserted, CacheWriteOutcome::Inserted(CacheVersion(1)));

        let conflict = store
            .compare_and_swap(key, CacheCondition::Absent, entry, deadline)
            .await
            .unwrap();
        assert_eq!(conflict, CacheWriteOutcome::Conflict(Some(CacheVersion(1))));
        assert_eq!(store.stats().conflicts, 1);
    }

    #[tokio::test]
    async fn fake_cache_single_flight_assigns_exactly_one_leader() {
        let (_, _, _, deadline) = fixtures();
        let store = FakeCacheStore::default();
        let key = CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(&b"single-flight.example/A"[..]),
            format_version: 1,
        };

        let (first, second, third) = tokio::join!(
            store.reserve_load(key.clone(), deadline),
            store.reserve_load(key.clone(), deadline),
            store.reserve_load(key, deadline),
        );

        let mut leaders = 0;
        let mut followers = 0;
        for reservation in [first.unwrap(), second.unwrap(), third.unwrap()] {
            match reservation {
                CacheLoadReservation::Leader(_) => leaders += 1,
                CacheLoadReservation::Follower(_) => followers += 1,
            }
        }
        assert_eq!(leaders, 1);
        assert_eq!(followers, 2);
    }

    #[tokio::test]
    async fn fake_cache_single_flight_shares_published_result_after_waiter_cancellation() {
        let (_, response, _, deadline) = fixtures();
        let store = Arc::new(FakeCacheStore::default());
        let key = CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(&b"single-flight.example/A"[..]),
            format_version: 1,
        };
        let leader = match store.reserve_load(key.clone(), deadline).await.unwrap() {
            CacheLoadReservation::Leader(lease) => lease,
            CacheLoadReservation::Follower(_) => panic!("first reservation must be the leader"),
        };
        let cancelled_waiter = match store.reserve_load(key.clone(), deadline).await.unwrap() {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("second reservation must be a follower"),
        };
        let shared_waiter = match store.reserve_load(key.clone(), deadline).await.unwrap() {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("third reservation must be a follower"),
        };

        let cancelled = Cancellation::new();
        let cancelled_store = Arc::clone(&store);
        let cancelled_task = tokio::spawn({
            let cancelled = cancelled.clone();
            async move {
                cancelled_store
                    .wait_load(cancelled_waiter, deadline, &cancelled)
                    .await
            }
        });

        let shared_cancellation = Cancellation::new();
        let shared_store = Arc::clone(&store);
        let shared_task = tokio::spawn({
            let shared_cancellation = shared_cancellation.clone();
            async move {
                shared_store
                    .wait_load(shared_waiter, deadline, &shared_cancellation)
                    .await
            }
        });

        tokio::task::yield_now().await;
        cancelled.cancel(CancelReason::ClientDisconnected);
        let cancelled_error = cancelled_task.await.unwrap().unwrap_err();
        assert!(matches!(
            cancelled_error.class(),
            PortErrorClass::Cancelled(CancelReason::ClientDisconnected)
        ));

        let still_waiting = match store.reserve_load(key, deadline).await.unwrap() {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => {
                panic!("a cancelled follower must not cancel the shared leader load")
            }
        };
        let now = Instant::now();
        let record = CacheRecord {
            version: CacheVersion(9),
            entry: Arc::new(CacheEntry {
                response: Arc::new(response),
                upstream:
                    crate::ports::cache::CacheUpstreamProvenance::direct_from_validated_config_id(
                        "test-upstream",
                    )
                    .unwrap(),
                inserted_at: now,
                expires_at: now + Duration::from_secs(60),
                stale_until: None,
                response_class: CacheResponseClass::NxDomain,
                producer_revision: RuntimeRevision(7),
                quality: CacheQuality::Negative,
                checksum: 9,
                format_version: crate::ports::cache::CACHE_ENTRY_FORMAT_VERSION,
            }),
        };
        store
            .publish_load(leader, CacheLoadCompletion::Ready(record.clone()), deadline)
            .await
            .unwrap();

        let shared = shared_task.await.unwrap().unwrap();
        let remaining_cancellation = Cancellation::new();
        let remaining = store
            .wait_load(still_waiting, deadline, &remaining_cancellation)
            .await
            .unwrap();
        for completion in [shared, remaining] {
            match completion {
                CacheLoadCompletion::Ready(received) => {
                    assert_eq!(received.version, record.version);
                    assert_eq!(received.entry.checksum, record.entry.checksum);
                }
                other => panic!("expected shared ready result, received {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn fake_cache_explicit_abandon_wakes_followers_with_shared_failure() {
        let (_, _, _, deadline) = fixtures();
        let store = Arc::new(FakeCacheStore::default());
        let key = CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(&b"abandon.example/A"[..]),
            format_version: 1,
        };
        let leader = match store.reserve_load(key.clone(), deadline).await.unwrap() {
            CacheLoadReservation::Leader(lease) => lease,
            CacheLoadReservation::Follower(_) => panic!("first reservation must be the leader"),
        };
        let waiter = match store.reserve_load(key, deadline).await.unwrap() {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("second reservation must be a follower"),
        };
        let waiting_store = Arc::clone(&store);
        let cancellation = Cancellation::new();
        let waiting = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                waiting_store
                    .wait_load(waiter, deadline, &cancellation)
                    .await
            }
        });

        tokio::task::yield_now().await;
        store
            .abandon_load(leader, CacheLoadFailure::Unavailable, deadline)
            .await
            .unwrap();
        assert!(matches!(
            waiting.await.unwrap().unwrap(),
            CacheLoadCompletion::Failed(CacheLoadFailure::Unavailable)
        ));
    }

    #[tokio::test]
    async fn fake_cache_dropped_leader_releases_followers_as_abandoned() {
        let (_, _, _, deadline) = fixtures();
        let store = Arc::new(FakeCacheStore::default());
        let key = CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(&b"abandoned-leader.example/A"[..]),
            format_version: 1,
        };
        let leader = match store.reserve_load(key.clone(), deadline).await.unwrap() {
            CacheLoadReservation::Leader(lease) => lease,
            CacheLoadReservation::Follower(_) => panic!("first reservation must be the leader"),
        };
        let waiter = match store.reserve_load(key, deadline).await.unwrap() {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("second reservation must be a follower"),
        };
        let waiting_store = Arc::clone(&store);
        let cancellation = Cancellation::new();
        let waiting = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                waiting_store
                    .wait_load(waiter, deadline, &cancellation)
                    .await
            }
        });

        tokio::task::yield_now().await;
        drop(leader);
        assert!(matches!(
            waiting.await.unwrap().unwrap(),
            CacheLoadCompletion::Failed(CacheLoadFailure::Abandoned)
        ));
    }

    #[tokio::test]
    async fn fake_cache_dropped_wait_future_releases_its_follower_slot() {
        let (_, _, _, deadline) = fixtures();
        let store = Arc::new(FakeCacheStore::default());
        let key = CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(&b"dropped-waiter.example/A"[..]),
            format_version: 1,
        };
        let leader = match store.reserve_load(key.clone(), deadline).await.unwrap() {
            CacheLoadReservation::Leader(lease) => lease,
            CacheLoadReservation::Follower(_) => panic!("first reservation must be the leader"),
        };
        let waiter = match store.reserve_load(key.clone(), deadline).await.unwrap() {
            CacheLoadReservation::Follower(waiter) => waiter,
            CacheLoadReservation::Leader(_) => panic!("second reservation must be a follower"),
        };
        let waiting_store = Arc::clone(&store);
        let cancellation = Cancellation::new();
        let waiting = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                waiting_store
                    .wait_load(waiter, deadline, &cancellation)
                    .await
            }
        });

        tokio::task::yield_now().await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        store
            .abandon_load(leader, CacheLoadFailure::Internal, deadline)
            .await
            .unwrap();
        let next = store.reserve_load(key, deadline).await.unwrap();
        assert!(matches!(next, CacheLoadReservation::Leader(_)));
    }

    #[test]
    fn fake_telemetry_rejects_unknown_high_cardinality_and_sensitive_labels() {
        assert_eq!(
            FakeTelemetry::validate_raw_label("tenant_free_form"),
            Err(TelemetryFieldError::UnknownLabel)
        );
        assert_eq!(
            FakeTelemetry::validate_raw_label("qname"),
            Err(TelemetryFieldError::SensitiveKey)
        );
        assert_eq!(
            FakeTelemetry::validate_raw_label("authorization"),
            Err(TelemetryFieldError::SensitiveKey)
        );
    }

    #[allow(dead_code)]
    fn dns_request_fixture() -> DnsRequest {
        let (query, _, context, _) = fixtures();
        DnsRequest { query, context }
    }
}
