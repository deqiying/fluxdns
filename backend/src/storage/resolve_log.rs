use std::fmt;
use std::net::IpAddr;
use std::time::SystemTime;

use crate::dns::{CancelReason, RuntimeRevision, TransportClass};
use crate::ports::observation::{ResolutionEvent, ResolutionTerminal};
use crate::ports::storage::{ResolveAnswer, ResolveEvent, ResolveRuleSource, StatsSource};
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
    dns_core_duration_micros: u64,
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
    /// 在详情 worker 内把共享解析事件投影为可持久化记录。
    pub(crate) fn from_resolution_event(event: &ResolutionEvent) -> Result<Self, PortError> {
        let detail = event.detail.as_ref().ok_or_else(|| {
            PortError::new(PortErrorClass::InvalidInput, "resolve detail projector")
                .with_safe_context("detail source is disabled")
        })?;
        let answers = detail.response.as_ref().map_or_else(Vec::new, |response| {
            response
                .as_message()
                .answers
                .iter()
                .map(|record| ResolveAnswer {
                    name: record.name.to_ascii(),
                    record_type: record.record_type().to_string(),
                    data: record.data.to_string(),
                    ttl: record.ttl,
                })
                .collect()
        });
        let (rcode, cancellation_reason) = match event.terminal {
            ResolutionTerminal::Response { rcode, .. } => (
                u8::try_from(rcode & 0x0f).expect("4-bit RCODE must fit u8"),
                None,
            ),
            ResolutionTerminal::NoResponse { reason } => (0, reason),
            ResolutionTerminal::CoreFailure => (0, None),
        };
        Self::from_event(ResolveEvent {
            occurred_at: event.occurred_at,
            duration_millis: event.duration_millis,
            dns_core_duration_micros: event.dns_core_duration_micros,
            request_digest: std::sync::Arc::from(format!("{:032x}", detail.request_id.0)),
            listener_id: std::sync::Arc::clone(&event.listener_id),
            route_id: event.route_id.clone(),
            client_ip: detail.client_ip,
            client_bucket: event.client_bucket.clone(),
            strategy_id: event.strategy_id.clone(),
            upstream_id: event.upstream_id.clone(),
            upstream_member_id: event.upstream_member_id.clone(),
            upstream_used_id: event.upstream_used_id.clone(),
            matched_rule_source: event.matched_rule_source,
            matched_resource_id: event.matched_resource_id.clone(),
            matched_rule_ordinal: event.matched_rule_ordinal,
            resource_version: event.resource_version,
            transport: event.transport,
            qname: std::sync::Arc::from(detail.question.name().to_ascii()),
            qtype: u16::from(detail.question.query_type()),
            qclass: u16::from(detail.question.query_class()),
            answers,
            rcode,
            cancellation_reason,
            outcome: event.outcome,
            source: event.source,
            cache_status: event.cache_lookup_status,
            runtime_revision: event.runtime_revision,
        })
    }

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
        let answer_count = u32::try_from(event.answers.len()).unwrap_or(u32::MAX);
        let (answers, answers_truncated) = bounded_answers(event.answers)?;

        Ok(Self {
            occurred_at: event.occurred_at,
            duration_millis: event.duration_millis,
            dns_core_duration_micros: event.dns_core_duration_micros,
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

    pub const fn dns_core_duration_micros(&self) -> u64 {
        self.dns_core_duration_micros
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
            .field("dns_core_duration_micros", &self.dns_core_duration_micros)
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
    use std::sync::Arc;
    use std::time::SystemTime;

    use crate::dns::{CancelReason, RuntimeRevision, TransportClass};
    use crate::ports::PortErrorClass;
    use crate::ports::storage::{ResolveAnswer, ResolveEvent, ResolveRuleSource, StatsSource};
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::resource::ResourceVersion;

    use super::ResolveDetailRecord;

    fn event(listener_id: &str) -> ResolveEvent {
        ResolveEvent {
            occurred_at: SystemTime::UNIX_EPOCH,
            duration_millis: 8,
            dns_core_duration_micros: 250,
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
        let record = ResolveDetailRecord::from_event(event("listener-public")).unwrap();
        assert_eq!(record.listener_id(), "listener-public");
        assert_eq!(record.duration_millis(), 8);
        assert_eq!(record.dns_core_duration_micros(), 250);
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
    fn listener_id_and_rcode_are_validated() {
        assert!(matches!(
            ResolveDetailRecord::from_event(event("bad listener value")),
            Err(error) if matches!(error.class(), PortErrorClass::InvalidInput)
        ));

        let mut invalid_rcode = event("listener");
        invalid_rcode.rcode = 16;
        assert!(matches!(
            ResolveDetailRecord::from_event(invalid_rcode),
            Err(error) if matches!(error.class(), PortErrorClass::InvalidInput)
        ));
    }
}
