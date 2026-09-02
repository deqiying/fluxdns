use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::dns::{RuntimeRevision, TransportClass};
use crate::ports::storage::{ResolveEvent, ResolveEventDisposition, ResolveEventSink};
use crate::ports::telemetry::{CacheStatus, OutcomeClass};
use crate::ports::{PortError, PortErrorClass};

const MAX_LISTENER_ID_BYTES: usize = 128;

/// 可持久化的解析详情摘要。
///
/// 只保留低基数标识和固定大小的请求摘要；原始域名、request digest、route/client/
/// strategy 文本都不进入此结构，避免详情 writer 成为敏感数据的旁路。
#[derive(Clone)]
pub struct ResolveDetailRecord {
    occurred_at: SystemTime,
    duration_millis: u64,
    listener_id: String,
    has_route: bool,
    has_client_bucket: bool,
    has_strategy: bool,
    has_request_digest: bool,
    transport: TransportClass,
    qname_byte_len: u16,
    qtype: u16,
    qclass: u16,
    outcome: OutcomeClass,
    cache_status: CacheStatus,
    runtime_revision: RuntimeRevision,
}

impl ResolveDetailRecord {
    pub(crate) fn from_event(event: ResolveEvent) -> Result<Self, PortError> {
        validate_listener_id(&event.listener_id)?;
        let duration_millis =
            u64::try_from(event.duration_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let qname_byte_len = u16::try_from(event.qname.len()).unwrap_or(u16::MAX);

        Ok(Self {
            occurred_at: event.occurred_at,
            duration_millis,
            listener_id: event.listener_id.to_string(),
            has_route: event.route_id.is_some(),
            has_client_bucket: event.client_bucket.is_some(),
            has_strategy: event.strategy_id.is_some(),
            has_request_digest: !event.request_digest.is_empty(),
            transport: event.transport,
            qname_byte_len,
            qtype: event.qtype,
            qclass: event.qclass,
            outcome: event.outcome,
            cache_status: event.cache_status,
            runtime_revision: event.runtime_revision,
        })
    }

    pub const fn occurred_at(&self) -> SystemTime {
        self.occurred_at
    }

    pub const fn duration_millis(&self) -> u64 {
        self.duration_millis
    }

    pub fn listener_id(&self) -> &str {
        &self.listener_id
    }

    pub const fn has_route(&self) -> bool {
        self.has_route
    }

    pub const fn has_client_bucket(&self) -> bool {
        self.has_client_bucket
    }

    pub const fn has_strategy(&self) -> bool {
        self.has_strategy
    }

    pub const fn has_request_digest(&self) -> bool {
        self.has_request_digest
    }

    pub const fn transport(&self) -> TransportClass {
        self.transport
    }

    pub const fn qname_byte_len(&self) -> u16 {
        self.qname_byte_len
    }

    pub const fn qtype(&self) -> u16 {
        self.qtype
    }

    pub const fn qclass(&self) -> u16 {
        self.qclass
    }

    pub const fn outcome(&self) -> OutcomeClass {
        self.outcome
    }

    pub const fn cache_status(&self) -> CacheStatus {
        self.cache_status
    }

    pub const fn runtime_revision(&self) -> RuntimeRevision {
        self.runtime_revision
    }
}

impl fmt::Debug for ResolveDetailRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolveDetailRecord")
            .field("occurred_at", &self.occurred_at)
            .field("duration_millis", &self.duration_millis)
            .field("listener_id_byte_len", &self.listener_id.len())
            .field("has_route", &self.has_route)
            .field("has_client_bucket", &self.has_client_bucket)
            .field("has_strategy", &self.has_strategy)
            .field("has_request_digest", &self.has_request_digest)
            .field("transport", &self.transport)
            .field("qname_byte_len", &self.qname_byte_len)
            .field("qtype", &self.qtype)
            .field("qclass", &self.qclass)
            .field("outcome", &self.outcome)
            .field("cache_status", &self.cache_status)
            .field("runtime_revision", &self.runtime_revision)
            .finish()
    }
}

/// writer adapter 的最小输出端；失败时不得吞掉 pending record。
pub trait ResolveDetailWriter: Send {
    fn append(&mut self, record: &ResolveDetailRecord) -> Result<(), PortError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ResolveLogBuildError {
    #[error("resolve log queue capacity must be greater than zero")]
    ZeroCapacity,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResolveLogFlushSummary {
    /// 本次 flush 成功写入的记录数。
    pub committed: u64,
    /// flush 后仍等待重试的记录数。
    pub pending: usize,
    /// 生命周期内因队列满而丢弃的记录数。
    pub dropped_queue_full: u64,
    /// 生命周期内 sink 返回失败的次数。
    pub sink_failures: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResolveLogShutdownSummary {
    pub flush: ResolveLogFlushSummary,
    /// shutdown flush 后仍无法提交、因此明确丢弃的记录数。
    pub discarded_pending: u64,
}

struct ResolveLogState<W> {
    writer: W,
    pending: VecDeque<ResolveDetailRecord>,
    accepting: bool,
    dropped_queue_full: u64,
    sink_failures: u64,
}

/// 详情日志的纯内存、有界 writer 前端。
///
/// 队列满时丢弃新记录并累计计数。sink 失败保留队首供后续 flush 重试，失败不会增加
/// 队列容量；shutdown 会先 flush 一次，然后显式清点并丢弃剩余记录。
pub struct ResolveLogWriter<W> {
    enabled: bool,
    capacity: usize,
    state: Mutex<ResolveLogState<W>>,
}

impl<W> ResolveLogWriter<W>
where
    W: ResolveDetailWriter,
{
    pub fn new(enabled: bool, capacity: usize, writer: W) -> Result<Self, ResolveLogBuildError> {
        if capacity == 0 {
            return Err(ResolveLogBuildError::ZeroCapacity);
        }
        Ok(Self {
            enabled,
            capacity,
            state: Mutex::new(ResolveLogState {
                writer,
                pending: VecDeque::with_capacity(capacity),
                accepting: true,
                dropped_queue_full: 0,
                sink_failures: 0,
            }),
        })
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn pending_len(&self) -> usize {
        self.state
            .lock()
            .expect("resolve log state lock poisoned")
            .pending
            .len()
    }

    pub fn flush(&self) -> ResolveLogFlushSummary {
        let mut state = self.state.lock().expect("resolve log state lock poisoned");
        let mut committed = 0;
        while let Some(record) = state.pending.front().cloned() {
            match state.writer.append(&record) {
                Ok(()) => {
                    state.pending.pop_front();
                    committed += 1;
                }
                Err(_) => {
                    state.sink_failures = state.sink_failures.saturating_add(1);
                    break;
                }
            }
        }
        summary(&state, committed)
    }

    pub fn shutdown(&self) -> ResolveLogShutdownSummary {
        let mut state = self.state.lock().expect("resolve log state lock poisoned");
        state.accepting = false;

        let mut committed = 0;
        while let Some(record) = state.pending.front().cloned() {
            match state.writer.append(&record) {
                Ok(()) => {
                    state.pending.pop_front();
                    committed += 1;
                }
                Err(_) => {
                    state.sink_failures = state.sink_failures.saturating_add(1);
                    break;
                }
            }
        }

        let discarded_pending = state.pending.len() as u64;
        state.pending.clear();
        ResolveLogShutdownSummary {
            flush: summary(&state, committed),
            discarded_pending,
        }
    }

    fn try_record_inner(&self, event: ResolveEvent) -> Result<ResolveEventDisposition, PortError> {
        if !self.enabled {
            return Ok(ResolveEventDisposition::Disabled);
        }
        let record = ResolveDetailRecord::from_event(event)?;
        let mut state = self.state.lock().expect("resolve log state lock poisoned");
        if !state.accepting {
            return Err(
                PortError::new(PortErrorClass::Unavailable, "resolve detail writer")
                    .with_safe_context("shutdown"),
            );
        }
        if state.pending.len() == self.capacity {
            state.dropped_queue_full = state.dropped_queue_full.saturating_add(1);
            return Ok(ResolveEventDisposition::DroppedQueueFull);
        }
        state.pending.push_back(record);
        Ok(ResolveEventDisposition::Accepted)
    }
}

impl<W> ResolveEventSink for ResolveLogWriter<W>
where
    W: ResolveDetailWriter,
{
    fn try_record(&self, event: ResolveEvent) -> Result<ResolveEventDisposition, PortError> {
        self.try_record_inner(event)
    }
}

fn summary<W>(state: &ResolveLogState<W>, committed: u64) -> ResolveLogFlushSummary {
    ResolveLogFlushSummary {
        committed,
        pending: state.pending.len(),
        dropped_queue_full: state.dropped_queue_full,
        sink_failures: state.sink_failures,
    }
}

fn validate_listener_id(listener_id: &str) -> Result<(), PortError> {
    if listener_id.is_empty()
        || listener_id.len() > MAX_LISTENER_ID_BYTES
        || !listener_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(
            PortError::new(PortErrorClass::InvalidInput, "resolve detail writer")
                .with_safe_context("invalid listener id"),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Instant, SystemTime};

    use crate::dns::{RuntimeRevision, TransportClass};
    use crate::ports::storage::{ResolveEvent, ResolveEventDisposition, ResolveEventSink};
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::ports::{PortError, PortErrorClass};

    use super::{ResolveDetailRecord, ResolveDetailWriter, ResolveLogBuildError, ResolveLogWriter};

    #[derive(Clone)]
    struct CapturingWriter {
        records: Arc<Mutex<Vec<ResolveDetailRecord>>>,
        failures_remaining: Arc<AtomicUsize>,
    }

    impl CapturingWriter {
        fn new(failures: usize) -> (Self, Arc<Mutex<Vec<ResolveDetailRecord>>>) {
            let records = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    records: Arc::clone(&records),
                    failures_remaining: Arc::new(AtomicUsize::new(failures)),
                },
                records,
            )
        }
    }

    impl ResolveDetailWriter for CapturingWriter {
        fn append(&mut self, record: &ResolveDetailRecord) -> Result<(), PortError> {
            if self
                .failures_remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "test resolve detail sink",
                ));
            }
            self.records
                .lock()
                .expect("test records lock poisoned")
                .push(record.clone());
            Ok(())
        }
    }

    fn event(listener_id: &str) -> ResolveEvent {
        ResolveEvent {
            occurred_at: SystemTime::UNIX_EPOCH,
            duration_started_at: Instant::now(),
            request_digest: Arc::from("request-digest-do-not-store"),
            listener_id: Arc::from(listener_id),
            route_id: Some(Arc::from("route-private-id")),
            client_bucket: Some(Arc::from("client-private-bucket")),
            strategy_id: Some(Arc::from("strategy-private-id")),
            transport: TransportClass::Datagram,
            qname: Arc::from("private.example.test."),
            qtype: 1,
            qclass: 1,
            outcome: OutcomeClass::Success,
            cache_status: CacheStatus::Miss,
            runtime_revision: RuntimeRevision(7),
        }
    }

    #[test]
    fn record_keeps_only_summary_fields_and_redacts_debug_output() {
        let (sink, records) = CapturingWriter::new(0);
        let writer = ResolveLogWriter::new(true, 2, sink).unwrap();
        assert!(matches!(
            writer.try_record(event("listener-public")),
            Ok(ResolveEventDisposition::Accepted)
        ));
        assert_eq!(writer.flush().committed, 1);

        let record = records.lock().unwrap().pop().unwrap();
        assert_eq!(record.listener_id(), "listener-public");
        assert_eq!(record.qname_byte_len(), 21);
        assert!(record.has_route());
        assert!(record.has_client_bucket());
        assert!(record.has_strategy());
        assert!(record.has_request_digest());
        let debug = format!("{record:?}");
        for sensitive in [
            "listener-public",
            "request-digest-do-not-store",
            "route-private-id",
            "client-private-bucket",
            "strategy-private-id",
            "private.example.test",
        ] {
            assert!(!debug.contains(sensitive));
        }
    }

    #[test]
    fn full_queue_drops_new_records_and_reports_total() {
        let (sink, records) = CapturingWriter::new(0);
        let writer = ResolveLogWriter::new(true, 1, sink).unwrap();
        assert!(matches!(
            writer.try_record(event("listener")),
            Ok(ResolveEventDisposition::Accepted)
        ));
        assert!(matches!(
            writer.try_record(event("listener")),
            Ok(ResolveEventDisposition::DroppedQueueFull)
        ));
        assert_eq!(writer.pending_len(), 1);

        let flushed = writer.flush();
        assert_eq!(flushed.committed, 1);
        assert_eq!(flushed.pending, 0);
        assert_eq!(flushed.dropped_queue_full, 1);
        assert_eq!(records.lock().unwrap().len(), 1);
    }

    #[test]
    fn sink_failure_keeps_pending_record_for_retry_without_growing_queue() {
        let (sink, records) = CapturingWriter::new(1);
        let writer = ResolveLogWriter::new(true, 1, sink).unwrap();
        writer.try_record(event("listener")).unwrap();

        let first = writer.flush();
        assert_eq!(first.committed, 0);
        assert_eq!(first.pending, 1);
        assert_eq!(first.sink_failures, 1);
        assert!(matches!(
            writer.try_record(event("listener")),
            Ok(ResolveEventDisposition::DroppedQueueFull)
        ));

        let second = writer.flush();
        assert_eq!(second.committed, 1);
        assert_eq!(second.pending, 0);
        assert_eq!(second.dropped_queue_full, 1);
        assert_eq!(records.lock().unwrap().len(), 1);
    }

    #[test]
    fn shutdown_flushes_then_discards_remaining_records_and_closes_sink() {
        let (sink, _) = CapturingWriter::new(1);
        let writer = ResolveLogWriter::new(true, 2, sink).unwrap();
        writer.try_record(event("listener")).unwrap();
        writer.try_record(event("listener")).unwrap();

        let summary = writer.shutdown();
        assert_eq!(summary.flush.committed, 0);
        assert_eq!(summary.flush.pending, 0);
        assert_eq!(summary.flush.sink_failures, 1);
        assert_eq!(summary.discarded_pending, 2);
        assert_eq!(writer.pending_len(), 0);
        assert!(matches!(
            writer.try_record(event("listener")),
            Err(error) if matches!(error.class(), PortErrorClass::Unavailable)
        ));
        assert_eq!(writer.shutdown().discarded_pending, 0);
    }

    #[test]
    fn disabled_writer_short_circuits_without_record_validation() {
        let (sink, records) = CapturingWriter::new(0);
        let writer = ResolveLogWriter::new(false, 1, sink).unwrap();
        assert!(matches!(
            writer.try_record(event("bad listener value")),
            Ok(ResolveEventDisposition::Disabled)
        ));
        assert_eq!(writer.pending_len(), 0);
        assert!(records.lock().unwrap().is_empty());
    }

    #[test]
    fn zero_capacity_is_rejected_and_listener_id_is_validated() {
        let (sink, _) = CapturingWriter::new(0);
        assert!(matches!(
            ResolveLogWriter::new(true, 0, sink),
            Err(ResolveLogBuildError::ZeroCapacity)
        ));

        let (sink, _) = CapturingWriter::new(0);
        let writer = ResolveLogWriter::new(true, 1, sink).unwrap();
        assert!(matches!(
            writer.try_record(event("bad listener value")),
            Err(error) if matches!(error.class(), PortErrorClass::InvalidInput)
        ));
    }
}
