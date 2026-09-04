use std::collections::VecDeque;
use std::fmt;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::dns::{CancelReason, RuntimeRevision, TransportClass};
use crate::ports::storage::{
    ResolveAnswer, ResolveEvent, ResolveEventDisposition, ResolveEventSink, ResolveRuleSource,
    StatsSource,
};
use crate::ports::telemetry::{CacheStatus, OutcomeClass};
use crate::ports::{PortError, PortErrorClass};
use crate::resource::ResourceVersion;

const MAX_LISTENER_ID_BYTES: usize = 128;
const MAX_CONFIGURED_ID_BYTES: usize = 128;
const MAX_QNAME_BYTES: usize = 1_024;
const MAX_ANSWER_NAME_BYTES: usize = 1_024;
const MAX_ANSWER_TYPE_BYTES: usize = 32;
const MAX_ANSWER_DATA_BYTES: usize = 512;
const MAX_ANSWER_COUNT: usize = 16;
const MAX_ANSWER_JSON_BYTES: usize = 4_096;

/// 可持久化的解析详情记录。
///
/// authenticated WebUI 需要展示完整查询上下文，因此保留 qname、有效 client IP、
/// strategy/upstream ID 和有界 answer 摘要；`Debug` 仍不得打印这些请求级内容。
#[derive(Clone)]
pub struct ResolveDetailRecord {
    occurred_at: SystemTime,
    duration_millis: u64,
    listener_id: String,
    has_route: bool,
    client_ip: Option<IpAddr>,
    client_bucket: Option<String>,
    strategy_id: Option<String>,
    upstream_id: Option<String>,
    upstream_member_id: Option<String>,
    upstream_used_id: Option<String>,
    matched_rule_source: Option<ResolveRuleSource>,
    has_matched_resource: bool,
    matched_rule_ordinal: Option<u64>,
    resource_version: Option<ResourceVersion>,
    has_request_digest: bool,
    transport: TransportClass,
    qname: String,
    qtype: u16,
    qclass: u16,
    answers: Vec<ResolveAnswer>,
    answer_count: u32,
    answers_truncated: bool,
    rcode: u8,
    cancellation_reason: Option<CancelReason>,
    outcome: OutcomeClass,
    source: StatsSource,
    cache_status: CacheStatus,
    runtime_revision: RuntimeRevision,
}

impl ResolveDetailRecord {
    pub(crate) fn from_event(event: ResolveEvent) -> Result<Self, PortError> {
        validate_listener_id(&event.listener_id)?;
        validate_optional_configured_id(event.client_bucket.as_deref(), "client bucket")?;
        validate_optional_configured_id(event.strategy_id.as_deref(), "strategy id")?;
        validate_optional_configured_id(event.upstream_id.as_deref(), "upstream id")?;
        validate_optional_configured_id(event.upstream_member_id.as_deref(), "upstream member id")?;
        validate_optional_configured_id(event.upstream_used_id.as_deref(), "upstream used id")?;
        if event.qname.is_empty() || event.qname.len() > MAX_QNAME_BYTES {
            return Err(
                PortError::new(PortErrorClass::InvalidInput, "resolve detail writer")
                    .with_safe_context("invalid canonical qname"),
            );
        }
        if event.rcode > 0x0f {
            return Err(
                PortError::new(PortErrorClass::InvalidInput, "resolve detail writer")
                    .with_safe_context("invalid DNS header RCODE"),
            );
        }
        let duration_millis =
            u64::try_from(event.duration_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let answer_count = u32::try_from(event.answers.len()).unwrap_or(u32::MAX);
        let (answers, answers_truncated) = bounded_answers(event.answers)?;

        Ok(Self {
            occurred_at: event.occurred_at,
            duration_millis,
            listener_id: event.listener_id.to_string(),
            has_route: event.route_id.is_some(),
            client_ip: event.client_ip,
            client_bucket: event.client_bucket.map(|value| value.to_string()),
            strategy_id: event.strategy_id.map(|value| value.to_string()),
            upstream_id: event.upstream_id.map(|value| value.to_string()),
            upstream_member_id: event.upstream_member_id.map(|value| value.to_string()),
            upstream_used_id: event.upstream_used_id.map(|value| value.to_string()),
            matched_rule_source: event.matched_rule_source,
            has_matched_resource: event.matched_resource_id.is_some(),
            matched_rule_ordinal: event.matched_rule_ordinal,
            resource_version: event.resource_version,
            has_request_digest: !event.request_digest.is_empty(),
            transport: event.transport,
            qname: event.qname.to_string(),
            qtype: event.qtype,
            qclass: event.qclass,
            answers,
            answer_count,
            answers_truncated,
            rcode: event.rcode,
            cancellation_reason: event.cancellation_reason,
            outcome: event.outcome,
            source: event.source,
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

    pub const fn client_ip(&self) -> Option<IpAddr> {
        self.client_ip
    }

    pub fn client_bucket(&self) -> Option<&str> {
        self.client_bucket.as_deref()
    }

    pub fn strategy_id(&self) -> Option<&str> {
        self.strategy_id.as_deref()
    }

    pub fn upstream_id(&self) -> Option<&str> {
        self.upstream_id.as_deref()
    }

    pub fn upstream_member_id(&self) -> Option<&str> {
        self.upstream_member_id.as_deref()
    }

    pub fn upstream_used_id(&self) -> Option<&str> {
        self.upstream_used_id.as_deref()
    }

    pub const fn matched_rule_source(&self) -> Option<ResolveRuleSource> {
        self.matched_rule_source
    }

    pub const fn has_matched_resource(&self) -> bool {
        self.has_matched_resource
    }

    pub const fn matched_rule_ordinal(&self) -> Option<u64> {
        self.matched_rule_ordinal
    }

    /// 返回命中资源的 epoch/revision；未命中资源时返回 `None`。
    pub const fn resource_version(&self) -> Option<ResourceVersion> {
        self.resource_version
    }

    pub const fn has_request_digest(&self) -> bool {
        self.has_request_digest
    }

    pub const fn transport(&self) -> TransportClass {
        self.transport
    }

    pub fn qname(&self) -> &str {
        &self.qname
    }

    pub const fn qtype(&self) -> u16 {
        self.qtype
    }

    pub const fn qclass(&self) -> u16 {
        self.qclass
    }

    pub fn answers(&self) -> &[ResolveAnswer] {
        &self.answers
    }

    pub const fn answer_count(&self) -> u32 {
        self.answer_count
    }

    pub const fn answers_truncated(&self) -> bool {
        self.answers_truncated
    }

    pub const fn rcode(&self) -> u8 {
        self.rcode
    }

    pub const fn cancellation_reason(&self) -> Option<CancelReason> {
        self.cancellation_reason
    }

    pub const fn outcome(&self) -> OutcomeClass {
        self.outcome
    }

    pub const fn source(&self) -> StatsSource {
        self.source
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
            .field("has_client_ip", &self.client_ip.is_some())
            .field("has_client_bucket", &self.client_bucket.is_some())
            .field("has_strategy", &self.strategy_id.is_some())
            .field("has_upstream", &self.upstream_id.is_some())
            .field("has_upstream_member", &self.upstream_member_id.is_some())
            .field("has_upstream_used", &self.upstream_used_id.is_some())
            .field("matched_rule_source", &self.matched_rule_source)
            .field("has_matched_resource", &self.has_matched_resource)
            .field("matched_rule_ordinal", &self.matched_rule_ordinal)
            .field("resource_version", &self.resource_version)
            .field("has_request_digest", &self.has_request_digest)
            .field("transport", &self.transport)
            .field("qname_byte_len", &self.qname.len())
            .field("qtype", &self.qtype)
            .field("qclass", &self.qclass)
            .field("answer_count", &self.answer_count)
            .field("stored_answer_count", &self.answers.len())
            .field("answers_truncated", &self.answers_truncated)
            .field("rcode", &self.rcode)
            .field("cancellation_reason", &self.cancellation_reason)
            .field("outcome", &self.outcome)
            .field("source", &self.source)
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

fn bounded_answers(source: Vec<ResolveAnswer>) -> Result<(Vec<ResolveAnswer>, bool), PortError> {
    let source_len = source.len();
    let mut truncated = source_len > MAX_ANSWER_COUNT;
    let mut answers = Vec::with_capacity(source_len.min(MAX_ANSWER_COUNT));
    for mut answer in source.into_iter().take(MAX_ANSWER_COUNT) {
        truncated |= truncate_utf8(&mut answer.name, MAX_ANSWER_NAME_BYTES);
        truncated |= truncate_utf8(&mut answer.record_type, MAX_ANSWER_TYPE_BYTES);
        truncated |= truncate_utf8(&mut answer.data, MAX_ANSWER_DATA_BYTES);
        answers.push(answer);
        let json_len = serde_json::to_vec(&answers)
            .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "resolve detail writer"))?
            .len();
        if json_len > MAX_ANSWER_JSON_BYTES {
            let _ = answers.pop();
            truncated = true;
            break;
        }
    }
    Ok((answers, truncated))
}

fn truncate_utf8(value: &mut String, max_bytes: usize) -> bool {
    if value.len() <= max_bytes {
        return false;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    true
}

fn validate_optional_configured_id(
    value: Option<&str>,
    field: &'static str,
) -> Result<(), PortError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty()
        || value.len() > MAX_CONFIGURED_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'!'))
    {
        return Err(
            PortError::new(PortErrorClass::InvalidInput, "resolve detail writer")
                .with_safe_context(field),
        );
    }
    Ok(())
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

    use crate::dns::{CancelReason, RuntimeRevision, TransportClass};
    use crate::ports::storage::{
        ResolveAnswer, ResolveEvent, ResolveEventDisposition, ResolveEventSink, ResolveRuleSource,
        StatsSource,
    };
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::ports::{PortError, PortErrorClass};
    use crate::resource::ResourceVersion;

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
            client_ip: Some("192.0.2.10".parse().unwrap()),
            client_bucket: Some(Arc::from("client-private-bucket")),
            strategy_id: Some(Arc::from("strategy-private-id")),
            upstream_id: Some(Arc::from("upstream-private-id")),
            upstream_member_id: Some(Arc::from("member-private-id")),
            upstream_used_id: Some(Arc::from("used-private-id")),
            matched_rule_source: Some(ResolveRuleSource::RuleSet),
            matched_resource_id: Some(Arc::from("resource-private-id")),
            matched_rule_ordinal: Some(3),
            resource_version: Some(ResourceVersion::new(2, 1)),
            transport: TransportClass::Datagram,
            qname: Arc::from("private.example.test."),
            qtype: 1,
            qclass: 1,
            answers: vec![ResolveAnswer {
                name: "private.example.test.".to_owned(),
                record_type: "A".to_owned(),
                data: "192.0.2.20".to_owned(),
                ttl: 60,
            }],
            rcode: 2,
            cancellation_reason: Some(CancelReason::Shutdown),
            outcome: OutcomeClass::Success,
            source: StatsSource::Upstream,
            cache_status: CacheStatus::Miss,
            runtime_revision: RuntimeRevision(7),
        }
    }

    #[test]
    fn record_keeps_query_details_and_redacts_debug_output() {
        let (sink, records) = CapturingWriter::new(0);
        let writer = ResolveLogWriter::new(true, 2, sink).unwrap();
        assert!(matches!(
            writer.try_record(event("listener-public")),
            Ok(ResolveEventDisposition::Accepted)
        ));
        assert_eq!(writer.flush().committed, 1);

        let record = records.lock().unwrap().pop().unwrap();
        assert_eq!(record.listener_id(), "listener-public");
        assert_eq!(record.qname(), "private.example.test.");
        assert!(record.has_route());
        assert_eq!(record.client_ip(), Some("192.0.2.10".parse().unwrap()));
        assert_eq!(record.client_bucket(), Some("client-private-bucket"));
        assert_eq!(record.strategy_id(), Some("strategy-private-id"));
        assert_eq!(record.upstream_id(), Some("upstream-private-id"));
        assert_eq!(record.upstream_member_id(), Some("member-private-id"));
        assert_eq!(record.upstream_used_id(), Some("used-private-id"));
        assert_eq!(record.answer_count(), 1);
        assert!(!record.answers_truncated());
        assert_eq!(record.answers()[0].data, "192.0.2.20");
        assert_eq!(
            record.matched_rule_source(),
            Some(ResolveRuleSource::RuleSet)
        );
        assert!(record.has_matched_resource());
        assert_eq!(record.matched_rule_ordinal(), Some(3));
        assert_eq!(record.resource_version(), Some(ResourceVersion::new(2, 1)));
        assert!(record.has_request_digest());
        assert_eq!(record.rcode(), 2);
        assert_eq!(record.cancellation_reason(), Some(CancelReason::Shutdown));
        assert_eq!(record.source(), StatsSource::Upstream);
        let debug = format!("{record:?}");
        for sensitive in [
            "listener-public",
            "request-digest-do-not-store",
            "route-private-id",
            "client-private-bucket",
            "strategy-private-id",
            "upstream-private-id",
            "member-private-id",
            "used-private-id",
            "resource-private-id",
            "private.example.test",
            "192.0.2.10",
            "192.0.2.20",
        ] {
            assert!(!debug.contains(sensitive));
        }
    }

    #[test]
    fn answer_summary_preserves_total_and_enforces_entry_and_byte_limits() {
        let mut detail = event("listener");
        detail.answers = (0..17)
            .map(|index| ResolveAnswer {
                name: format!("answer-{index}.example."),
                record_type: "TXT".to_owned(),
                data: "界".repeat(300),
                ttl: 60,
            })
            .collect();

        let record = ResolveDetailRecord::from_event(detail).unwrap();
        assert_eq!(record.answer_count(), 17);
        assert!(record.answers_truncated());
        assert!(!record.answers().is_empty());
        assert!(record.answers().len() <= 16);
        assert!(
            record
                .answers()
                .iter()
                .all(|answer| answer.data.len() <= 512)
        );
        assert!(serde_json::to_vec(record.answers()).unwrap().len() <= 4_096);
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

        let mut invalid_rcode = event("listener");
        invalid_rcode.rcode = 16;
        assert!(matches!(
            writer.try_record(invalid_rcode),
            Err(error) if matches!(error.class(), PortErrorClass::InvalidInput)
        ));
    }
}
