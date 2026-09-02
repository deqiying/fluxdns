//! 业务存储、聚合统计和可选解析详情契约。

use std::fmt;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::dns::{CancelReason, Deadline, RuntimeRevision, TransportClass};
use crate::resource::ResourceVersion;

use super::telemetry::{CacheStatus, ConfiguredId, ConfiguredIdKind, OutcomeClass};
use super::{PortError, PortFuture};

#[cfg(test)]
use super::telemetry::{ConfiguredIdError, ConfiguredIdRegistry, ValidatedConfiguredId};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SchemaVersion(pub u32);

#[derive(Clone, Debug)]
pub enum StorageOperation {
    StatsBatch(StatsBatch),
    ResolveBatch(Vec<ResolveEvent>),
}

#[derive(Clone, Debug)]
pub struct StorageTransaction {
    pub idempotency_key: Arc<str>,
    pub operations: Vec<StorageOperation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageHealth {
    Healthy,
    Degraded,
    Failed,
    Stopping,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageFlushSummary {
    pub stats_committed: u64,
    pub details_committed: u64,
    pub details_dropped: u64,
    pub persistence_gap: bool,
}

pub trait StorageBackend: Send + Sync {
    fn migrate(
        &self,
        target: SchemaVersion,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<SchemaVersion, PortError>>;

    fn execute(
        &self,
        transaction: StorageTransaction,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(), PortError>>;

    fn health_probe(&self, deadline: Deadline) -> PortFuture<'_, Result<StorageHealth, PortError>>;

    fn checkpoint(&self, deadline: Deadline) -> PortFuture<'_, Result<(), PortError>>;

    fn flush(&self, deadline: Deadline) -> PortFuture<'_, Result<StorageFlushSummary, PortError>>;

    fn shutdown(
        &self,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<StorageFlushSummary, PortError>>;
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StatsDimensionKind {
    ClientBucket,
    Transport,
    Strategy,
    Source,
    Upstream,
    Rcode,
    CacheStatus,
    AttemptOutcome,
}

/// 经过构造时校验的聚合统计维度。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StatsDimension {
    kind: StatsDimensionKind,
    value: StatsDimensionValue,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum StatsDimensionValue {
    ConfiguredId(ConfiguredId),
    Transport(TransportClass),
    Source(StatsSource),
    Rcode(u16),
    CacheStatus(CacheStatus),
    AttemptOutcome(OutcomeClass),
}

impl StatsDimension {
    pub fn client_bucket(id: ConfiguredId) -> Result<Self, StatsEventError> {
        Self::configured(StatsDimensionKind::ClientBucket, id)
    }

    pub const fn transport(value: TransportClass) -> Self {
        Self {
            kind: StatsDimensionKind::Transport,
            value: StatsDimensionValue::Transport(value),
        }
    }

    pub fn strategy(id: ConfiguredId) -> Result<Self, StatsEventError> {
        Self::configured(StatsDimensionKind::Strategy, id)
    }

    pub const fn source(value: StatsSource) -> Self {
        Self {
            kind: StatsDimensionKind::Source,
            value: StatsDimensionValue::Source(value),
        }
    }

    pub fn upstream(id: ConfiguredId) -> Result<Self, StatsEventError> {
        Self::configured(StatsDimensionKind::Upstream, id)
    }

    pub const fn rcode(value: u16) -> Self {
        Self {
            kind: StatsDimensionKind::Rcode,
            value: StatsDimensionValue::Rcode(value),
        }
    }

    pub const fn cache_status(value: CacheStatus) -> Self {
        Self {
            kind: StatsDimensionKind::CacheStatus,
            value: StatsDimensionValue::CacheStatus(value),
        }
    }

    pub const fn attempt_outcome(value: OutcomeClass) -> Self {
        Self {
            kind: StatsDimensionKind::AttemptOutcome,
            value: StatsDimensionValue::AttemptOutcome(value),
        }
    }

    fn configured(kind: StatsDimensionKind, id: ConfiguredId) -> Result<Self, StatsEventError> {
        if !configured_id_kind_matches(kind, id.kind()) {
            return Err(StatsEventError::ConfiguredIdKindMismatch);
        }
        Ok(Self {
            kind,
            value: StatsDimensionValue::ConfiguredId(id),
        })
    }

    pub const fn kind(&self) -> StatsDimensionKind {
        self.kind
    }

    pub(crate) fn database_parts(&self) -> (&'static str, String) {
        let kind = match self.kind {
            StatsDimensionKind::ClientBucket => "client_bucket",
            StatsDimensionKind::Transport => "transport",
            StatsDimensionKind::Strategy => "strategy",
            StatsDimensionKind::Source => "source",
            StatsDimensionKind::Upstream => "upstream",
            StatsDimensionKind::Rcode => "rcode",
            StatsDimensionKind::CacheStatus => "cache_status",
            StatsDimensionKind::AttemptOutcome => "attempt_outcome",
        };
        let value = match &self.value {
            StatsDimensionValue::ConfiguredId(id) => id.as_str().to_owned(),
            StatsDimensionValue::Transport(value) => match value {
                crate::dns::TransportClass::Datagram => "datagram".to_owned(),
                crate::dns::TransportClass::Stream => "stream".to_owned(),
                crate::dns::TransportClass::Multiplexed => "multiplexed".to_owned(),
            },
            StatsDimensionValue::Source(value) => match value {
                StatsSource::Cache => "cache".to_owned(),
                StatsSource::Hosts => "hosts".to_owned(),
                StatsSource::RuleSet => "rule_set".to_owned(),
                StatsSource::Upstream => "upstream".to_owned(),
            },
            StatsDimensionValue::Rcode(value) => value.to_string(),
            StatsDimensionValue::CacheStatus(value) => match value {
                CacheStatus::Disabled => "disabled".to_owned(),
                CacheStatus::Miss => "miss".to_owned(),
                CacheStatus::Fresh => "fresh".to_owned(),
                CacheStatus::Stale => "stale".to_owned(),
                CacheStatus::StoreUnavailable => "store_unavailable".to_owned(),
                CacheStatus::WriteRejected => "write_rejected".to_owned(),
            },
            StatsDimensionValue::AttemptOutcome(value) => match value {
                OutcomeClass::Success => "success".to_owned(),
                OutcomeClass::Failure => "failure".to_owned(),
                OutcomeClass::Timeout => "timeout".to_owned(),
                OutcomeClass::Cancelled => "cancelled".to_owned(),
                OutcomeClass::Rejected => "rejected".to_owned(),
                OutcomeClass::Dropped => "dropped".to_owned(),
            },
        };
        (kind, value)
    }

    fn validate(&self) -> Result<(), StatsEventError> {
        match &self.value {
            StatsDimensionValue::ConfiguredId(id)
                if configured_id_kind_matches(self.kind, id.kind()) =>
            {
                Ok(())
            }
            StatsDimensionValue::ConfiguredId(_) => Err(StatsEventError::ConfiguredIdKindMismatch),
            StatsDimensionValue::Transport(_) if self.kind == StatsDimensionKind::Transport => {
                Ok(())
            }
            StatsDimensionValue::Source(_) if self.kind == StatsDimensionKind::Source => Ok(()),
            StatsDimensionValue::Rcode(_) if self.kind == StatsDimensionKind::Rcode => Ok(()),
            StatsDimensionValue::CacheStatus(_) if self.kind == StatsDimensionKind::CacheStatus => {
                Ok(())
            }
            StatsDimensionValue::AttemptOutcome(_)
                if self.kind == StatsDimensionKind::AttemptOutcome =>
            {
                Ok(())
            }
            _ => Err(StatsEventError::InvalidDimension),
        }
    }
}

const fn configured_id_kind_matches(
    dimension: StatsDimensionKind,
    configured: ConfiguredIdKind,
) -> bool {
    matches!(
        (dimension, configured),
        (
            StatsDimensionKind::ClientBucket,
            ConfiguredIdKind::ClientBucket
        ) | (StatsDimensionKind::Strategy, ConfiguredIdKind::Strategy)
            | (StatsDimensionKind::Upstream, ConfiguredIdKind::Upstream)
    )
}

impl StatsDimensionKind {
    const fn index(self) -> usize {
        match self {
            Self::ClientBucket => 0,
            Self::Transport => 1,
            Self::Strategy => 2,
            Self::Source => 3,
            Self::Upstream => 4,
            Self::Rcode => 5,
            Self::CacheStatus => 6,
            Self::AttemptOutcome => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StatsSource {
    Cache,
    Hosts,
    RuleSet,
    Upstream,
}

/// 详情记录中的规则来源；不携带规则文本或 matcher 内容。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveRuleSource {
    ListenerHosts,
    StrategyHosts,
    RuleSet,
}

pub const MAX_STATS_DIMENSIONS: usize = 8;

#[derive(Clone, Debug)]
pub struct StatsEvent {
    sequence: u64,
    day_utc: i32,
    dimensions: Vec<StatsDimension>,
}

impl StatsEvent {
    pub fn new(
        sequence: u64,
        day_utc: i32,
        dimensions: Vec<StatsDimension>,
    ) -> Result<Self, StatsEventError> {
        if dimensions.len() > MAX_STATS_DIMENSIONS {
            return Err(StatsEventError::TooManyDimensions);
        }

        let mut seen = [false; MAX_STATS_DIMENSIONS];
        for dimension in &dimensions {
            dimension.validate()?;
            let index = dimension.kind().index();
            if seen[index] {
                return Err(StatsEventError::DuplicateDimension);
            }
            seen[index] = true;
        }

        Ok(Self {
            sequence,
            day_utc,
            dimensions,
        })
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn day_utc(&self) -> i32 {
        self.day_utc
    }

    pub fn dimensions(&self) -> &[StatsDimension] {
        &self.dimensions
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StatsEventError {
    #[error("stats dimension contains an invalid value")]
    InvalidDimension,
    #[error("configured id kind does not match its stats dimension")]
    ConfiguredIdKindMismatch,
    #[error("stats event contains duplicate dimensions")]
    DuplicateDimension,
    #[error("stats event contains too many dimensions")]
    TooManyDimensions,
}

#[derive(Clone, Debug)]
pub struct StatsBatch {
    pub batch_id: u64,
    pub max_event_sequence: u64,
    pub counter_epoch: u64,
    pub events: Vec<StatsEvent>,
}

pub trait StatsRecorder: Send + Sync {
    /// 热路径同步记录；容量不足必须显式返回错误，不能静默丢弃。
    fn record(&self, event: StatsEvent) -> Result<(), PortError>;
}

#[derive(Clone)]
pub struct ResolveEvent {
    pub occurred_at: SystemTime,
    pub duration_started_at: Instant,
    pub request_digest: Arc<str>,
    pub listener_id: Arc<str>,
    pub route_id: Option<Arc<str>>,
    pub client_bucket: Option<Arc<str>>,
    pub strategy_id: Option<Arc<str>>,
    /// 策略选中的 direct upstream 或 group ID。
    pub upstream_id: Option<Arc<str>>,
    /// group 实际选中的顶层成员 ID。
    pub upstream_member_id: Option<Arc<str>>,
    /// 命中规则的低基数来源，不包含 matcher 或规则文本。
    pub matched_rule_source: Option<ResolveRuleSource>,
    /// 命中 hosts/rule-set 的已验证资源 ID。
    pub matched_resource_id: Option<Arc<str>>,
    /// strategy 规则序号；listener hosts 没有序号。
    pub matched_rule_ordinal: Option<u64>,
    /// 命中资源的 epoch/revision。
    pub resource_version: Option<ResourceVersion>,
    pub transport: TransportClass,
    pub qname: Arc<str>,
    pub qtype: u16,
    pub qclass: u16,
    /// DNS header 的 4-bit RCODE；无 DNS response 时为 0，并由 outcome/failure 分类区分。
    pub rcode: u8,
    /// 请求结束时已记录的首个协作式取消原因。
    pub cancellation_reason: Option<CancelReason>,
    pub outcome: OutcomeClass,
    pub source: StatsSource,
    pub cache_status: CacheStatus,
    pub runtime_revision: RuntimeRevision,
}

impl fmt::Debug for ResolveEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolveEvent")
            .field("occurred_at", &self.occurred_at)
            .field("duration_started_at", &self.duration_started_at)
            .field("has_request_digest", &!self.request_digest.is_empty())
            .field("listener_id", &self.listener_id)
            .field("has_route_id", &self.route_id.is_some())
            .field("has_client_bucket", &self.client_bucket.is_some())
            .field("has_strategy_id", &self.strategy_id.is_some())
            .field("has_upstream_id", &self.upstream_id.is_some())
            .field("has_upstream_member_id", &self.upstream_member_id.is_some())
            .field("matched_rule_source", &self.matched_rule_source)
            .field(
                "has_matched_resource_id",
                &self.matched_resource_id.is_some(),
            )
            .field("matched_rule_ordinal", &self.matched_rule_ordinal)
            .field("resource_version", &self.resource_version)
            .field("transport", &self.transport)
            .field("qname_byte_len", &self.qname.len())
            .field("qtype", &self.qtype)
            .field("qclass", &self.qclass)
            .field("rcode", &self.rcode)
            .field("cancellation_reason", &self.cancellation_reason)
            .field("outcome", &self.outcome)
            .field("source", &self.source)
            .field("cache_status", &self.cache_status)
            .field("runtime_revision", &self.runtime_revision)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_event_accepts_only_unique_typed_dimensions() {
        let registry = ConfiguredIdRegistry::from_validated([ValidatedConfiguredId::new(
            ConfiguredIdKind::Strategy,
            "primary",
        )
        .unwrap()]);
        let strategy = registry
            .issue(ConfiguredIdKind::Strategy, "primary")
            .unwrap();
        let event = StatsEvent::new(
            1,
            20_260_830,
            vec![
                StatsDimension::transport(TransportClass::Datagram),
                StatsDimension::strategy(strategy.clone()).unwrap(),
            ],
        )
        .unwrap();

        assert_eq!(event.sequence(), 1);
        assert_eq!(event.dimensions().len(), 2);
        assert_eq!(
            StatsEvent::new(
                2,
                20_260_830,
                vec![
                    StatsDimension::strategy(strategy.clone()).unwrap(),
                    StatsDimension::strategy(strategy).unwrap(),
                ],
            )
            .unwrap_err(),
            StatsEventError::DuplicateDimension
        );
    }

    #[test]
    fn configured_stats_dimensions_require_registered_and_matching_ids() {
        let registry = ConfiguredIdRegistry::from_validated([
            ValidatedConfiguredId::new(ConfiguredIdKind::ClientBucket, "office-network").unwrap(),
            ValidatedConfiguredId::new(ConfiguredIdKind::Strategy, "primary").unwrap(),
            ValidatedConfiguredId::new(ConfiguredIdKind::Upstream, "remote").unwrap(),
        ]);

        for raw_request_value in ["private.example.test.", "client-7f3a9d"] {
            assert_eq!(
                registry.issue(ConfiguredIdKind::ClientBucket, raw_request_value),
                Err(ConfiguredIdError::UnknownConfigurationId)
            );
        }

        let strategy = registry
            .issue(ConfiguredIdKind::Strategy, "primary")
            .unwrap();
        assert_eq!(
            StatsDimension::client_bucket(strategy).unwrap_err(),
            StatsEventError::ConfiguredIdKindMismatch
        );

        let client_bucket = registry
            .issue(ConfiguredIdKind::ClientBucket, "office-network")
            .unwrap();
        let upstream = registry
            .issue(ConfiguredIdKind::Upstream, "remote")
            .unwrap();
        let event = StatsEvent::new(
            3,
            20_260_830,
            vec![
                StatsDimension::client_bucket(client_bucket).unwrap(),
                StatsDimension::upstream(upstream).unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(event.dimensions().len(), 2);
    }

    #[test]
    fn resolve_event_debug_does_not_expose_qname_or_client_details() {
        let event = ResolveEvent {
            occurred_at: SystemTime::UNIX_EPOCH,
            duration_started_at: Instant::now(),
            request_digest: Arc::from("request-digest-do-not-log"),
            listener_id: Arc::from("listener-public-id"),
            route_id: Some(Arc::from("route-private-id")),
            client_bucket: Some(Arc::from("client-private-bucket")),
            strategy_id: Some(Arc::from("strategy-private-id")),
            upstream_id: Some(Arc::from("upstream-private-id")),
            upstream_member_id: Some(Arc::from("member-private-id")),
            matched_rule_source: Some(ResolveRuleSource::RuleSet),
            matched_resource_id: Some(Arc::from("resource-private-id")),
            matched_rule_ordinal: Some(3),
            resource_version: Some(ResourceVersion::new(2, 1)),
            transport: TransportClass::Datagram,
            qname: Arc::from("private.example.test."),
            qtype: 1,
            qclass: 1,
            rcode: 5,
            cancellation_reason: Some(CancelReason::GroupPolicy),
            outcome: OutcomeClass::Success,
            source: StatsSource::Upstream,
            cache_status: CacheStatus::Miss,
            runtime_revision: RuntimeRevision(7),
        };

        let debug = format!("{event:?}");
        for sensitive in [
            "request-digest-do-not-log",
            "route-private-id",
            "client-private-bucket",
            "strategy-private-id",
            "upstream-private-id",
            "member-private-id",
            "resource-private-id",
            "private.example.test",
        ] {
            assert!(!debug.contains(sensitive));
        }
        assert!(debug.contains("listener-public-id"));
        assert!(debug.contains("qname_byte_len"));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveEventDisposition {
    Accepted,
    Disabled,
    DroppedQueueFull,
    DroppedByPolicy,
}

pub trait ResolveEventSink: Send + Sync {
    /// 详情记录允许按明确策略丢弃，调用方必须根据返回值累计计数。
    fn try_record(&self, event: ResolveEvent) -> Result<ResolveEventDisposition, PortError>;
}
