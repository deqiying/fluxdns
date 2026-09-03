//! 已脱敏日志、低基数 metrics 与组件健康状态契约。

#[cfg(test)]
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::dns::{Deadline, RuntimeRevision, TransportClass};

use super::{PortError, PortFuture};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Component {
    Application,
    Runtime,
    Listener,
    Dns,
    Policy,
    Upstream,
    Cache,
    Resource,
    Storage,
    Telemetry,
    Management,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OutcomeClass {
    Success,
    Failure,
    Timeout,
    Cancelled,
    Rejected,
    Dropped,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CacheStatus {
    Disabled,
    Miss,
    Fresh,
    Stale,
    StoreUnavailable,
    WriteRejected,
}

#[derive(Clone, Debug)]
pub struct LogEvent {
    pub occurred_at: SystemTime,
    pub level: LogLevel,
    pub name: EventName,
    pub component: Component,
    pub request_digest: Option<Arc<str>>,
    pub configured_id: Option<ConfiguredId>,
    pub outcome: OutcomeClass,
    pub runtime_revision: Option<RuntimeRevision>,
    pub message: &'static str,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EventName(Arc<str>);

impl EventName {
    pub fn parse(value: impl Into<Arc<str>>) -> Result<Self, TelemetryFieldError> {
        validate_bounded_identifier(value.into()).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 已通过配置校验并由注册表签发的低基数标识。
///
/// 该类型没有公开的字符串构造入口。运行时的 qname、client id 等原始数据只能
/// 在阶段 2 由真实配置边界验证并签发 token；阶段 1 不提供生产构造入口。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConfiguredId {
    kind: ConfiguredIdKind,
    value: Arc<str>,
}

impl ConfiguredId {
    pub const fn kind(&self) -> ConfiguredIdKind {
        self.kind
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }
}

/// 从已经通过配置边界校验的标识构造 telemetry 内部 token。
///
/// 生产调用方只能传入配置解析阶段产生的 bounded identifier；该入口保持 crate
/// 可见，避免把任意请求字段提升为 metrics label。
pub(crate) fn configured_id_from_validated(
    kind: ConfiguredIdKind,
    value: &str,
) -> Option<ConfiguredId> {
    validate_bounded_identifier(Arc::from(value))
        .ok()
        .map(|value| ConfiguredId { kind, value })
}

/// 配置项在 metrics、stats 中可出现的位置。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ConfiguredIdKind {
    ClientBucket,
    Strategy,
    Upstream,
    Listener,
    Route,
}

/// 测试夹具：模拟配置解析层完成校验后的受控输入。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg(test)]
pub(super) struct ValidatedConfiguredId(ConfiguredId);

#[cfg(test)]
impl ValidatedConfiguredId {
    pub(super) fn new(
        kind: ConfiguredIdKind,
        value: impl Into<Arc<str>>,
    ) -> Result<Self, ConfiguredIdError> {
        let value = validate_bounded_identifier(value.into())
            .map_err(|_| ConfiguredIdError::InvalidConfigurationId)?;
        Ok(Self(ConfiguredId { kind, value }))
    }
}

/// 测试夹具：只对测试中登记的配置值签发 token。
#[derive(Clone, Debug, Default)]
#[cfg(test)]
pub(super) struct ConfiguredIdRegistry {
    configured: BTreeSet<ConfiguredId>,
}

#[cfg(test)]
impl ConfiguredIdRegistry {
    pub(super) fn from_validated(ids: impl IntoIterator<Item = ValidatedConfiguredId>) -> Self {
        Self {
            configured: ids.into_iter().map(|id| id.0).collect(),
        }
    }

    /// 只为已登记配置签发 token，拒绝任何未在配置中出现的原始字符串。
    pub(super) fn issue(
        &self,
        kind: ConfiguredIdKind,
        value: &str,
    ) -> Result<ConfiguredId, ConfiguredIdError> {
        let candidate = ConfiguredId {
            kind,
            value: Arc::from(value),
        };
        self.configured
            .get(&candidate)
            .cloned()
            .ok_or(ConfiguredIdError::UnknownConfigurationId)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[cfg(test)]
pub(super) enum ConfiguredIdError {
    #[error("configured id is not a bounded safe identifier")]
    InvalidConfigurationId,
    #[error("configured id was not registered by validated configuration")]
    UnknownConfigurationId,
}

fn validate_bounded_identifier(value: Arc<str>) -> Result<Arc<str>, TelemetryFieldError> {
    if value.is_empty() || value.len() > 128 {
        return Err(TelemetryFieldError::InvalidValue);
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(TelemetryFieldError::InvalidValue);
    }
    Ok(value)
}

pub trait LogSink: Send + Sync {
    fn emit(&self, event: LogEvent) -> Result<(), PortError>;

    fn flush(&self, deadline: Deadline)
    -> PortFuture<'_, Result<TelemetryFlushSummary, PortError>>;
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MetricName {
    RequestsTotal,
    RequestsActive,
    RequestsCancelled,
    RequestsFailed,
    RequestLatency,
    CacheOperations,
    UpstreamAttempts,
    UpstreamLatency,
    ListenerConnections,
    ResourceRefresh,
    WriterQueueDepth,
    WriterDropped,
    WriterRetry,
    PersistenceGap,
    RuntimeRevision,
    ComponentHealth,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MetricLabelKey {
    Component,
    Transport,
    Outcome,
    CacheStatus,
    ConfiguredId,
}

impl MetricLabelKey {
    const COUNT: usize = 5;

    pub fn parse(value: &str) -> Result<Self, TelemetryFieldError> {
        match value {
            "component" => Ok(Self::Component),
            "transport" => Ok(Self::Transport),
            "outcome" => Ok(Self::Outcome),
            "cache_status" => Ok(Self::CacheStatus),
            "configured_id" => Ok(Self::ConfiguredId),
            value if is_sensitive_key(value) => Err(TelemetryFieldError::SensitiveKey),
            _ => Err(TelemetryFieldError::UnknownLabel),
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Component => 0,
            Self::Transport => 1,
            Self::Outcome => 2,
            Self::CacheStatus => 3,
            Self::ConfiguredId => 4,
        }
    }

    fn accepts(self, value: &MetricLabelValue) -> bool {
        matches!(
            (self, value),
            (Self::Component, MetricLabelValue::Component(_))
                | (Self::Transport, MetricLabelValue::Transport(_))
                | (Self::Outcome, MetricLabelValue::Outcome(_))
                | (Self::CacheStatus, MetricLabelValue::CacheStatus(_))
                | (Self::ConfiguredId, MetricLabelValue::ConfiguredId(_))
        )
    }
}

fn is_sensitive_key(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "secret"
            | "password"
            | "credential"
            | "authorization"
            | "qname"
            | "client_id"
            | "client_ip"
            | "ip"
            | "url"
            | "header"
            | "query"
            | "wire"
            | "error"
            | "error_message"
    )
}

#[derive(Clone, Debug)]
pub enum MetricLabelValue {
    Component(Component),
    Transport(TransportClass),
    Outcome(OutcomeClass),
    CacheStatus(CacheStatus),
    ConfiguredId(ConfiguredId),
}

#[derive(Clone, Debug)]
pub struct MetricLabel {
    key: MetricLabelKey,
    value: MetricLabelValue,
}

impl MetricLabel {
    pub fn new(key: MetricLabelKey, value: MetricLabelValue) -> Result<Self, TelemetryFieldError> {
        let label = Self { key, value };
        label.validate()?;
        Ok(label)
    }

    pub fn key(&self) -> MetricLabelKey {
        self.key
    }

    pub fn value(&self) -> &MetricLabelValue {
        &self.value
    }

    pub fn validate(&self) -> Result<(), TelemetryFieldError> {
        if !self.key.accepts(&self.value) {
            return Err(TelemetryFieldError::LabelTypeMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MetricValue {
    Counter(u64),
    Gauge(i64),
    DurationMicros(u64),
}

#[derive(Clone, Debug)]
pub struct MetricEvent {
    name: MetricName,
    labels: Vec<MetricLabel>,
    value: MetricValue,
}

pub const MAX_METRIC_LABELS: usize = MetricLabelKey::COUNT;

impl MetricEvent {
    pub fn new(
        name: MetricName,
        labels: Vec<MetricLabel>,
        value: MetricValue,
    ) -> Result<Self, TelemetryFieldError> {
        let event = Self {
            name,
            labels,
            value,
        };
        event.validate()?;
        Ok(event)
    }

    pub fn name(&self) -> MetricName {
        self.name
    }

    pub fn labels(&self) -> &[MetricLabel] {
        &self.labels
    }

    pub fn value(&self) -> MetricValue {
        self.value
    }

    pub fn validate(&self) -> Result<(), TelemetryFieldError> {
        if self.labels.len() > MAX_METRIC_LABELS {
            return Err(TelemetryFieldError::TooManyLabels);
        }

        let mut seen = [false; MetricLabelKey::COUNT];
        for label in &self.labels {
            label.validate()?;
            let index = label.key().index();
            if seen[index] {
                return Err(TelemetryFieldError::DuplicateLabel);
            }
            seen[index] = true;
        }
        Ok(())
    }
}

pub trait MetricsSink: Send + Sync {
    fn record(&self, event: MetricEvent) -> Result<(), PortError>;
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ComponentHealthState {
    Healthy,
    Degraded,
    Failed,
    Stopping,
}

#[derive(Clone, Debug)]
pub struct ComponentHealthEvent {
    pub component: Component,
    pub state: ComponentHealthState,
    pub first_seen: Instant,
    pub last_changed: Instant,
    pub last_success: Option<Instant>,
    pub retry_count: u64,
    pub stale_age_micros: Option<u64>,
    pub persistence_gap: bool,
    pub safe_reason: Option<&'static str>,
}

pub trait HealthSink: Send + Sync {
    fn update(&self, event: ComponentHealthEvent) -> Result<(), PortError>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TelemetryFlushSummary {
    pub emitted: u64,
    pub dropped_low_priority: u64,
    pub failed: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryFieldError {
    UnknownLabel,
    SensitiveKey,
    InvalidValue,
    LabelTypeMismatch,
    DuplicateLabel,
    TooManyLabels,
}

impl fmt::Display for TelemetryFieldError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnknownLabel => "unknown metric label",
            Self::SensitiveKey => "sensitive or high-cardinality metric label is forbidden",
            Self::InvalidValue => "telemetry field is not a bounded safe identifier",
            Self::LabelTypeMismatch => "metric label value does not match its key",
            Self::DuplicateLabel => "metric label key must be unique",
            Self::TooManyLabels => "metric event has too many labels",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for TelemetryFieldError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(key: MetricLabelKey, value: MetricLabelValue) -> MetricLabel {
        MetricLabel::new(key, value).unwrap()
    }

    #[test]
    fn metric_label_rejects_type_mismatch() {
        assert_eq!(
            MetricLabel::new(
                MetricLabelKey::Component,
                MetricLabelValue::Outcome(OutcomeClass::Success),
            )
            .unwrap_err(),
            TelemetryFieldError::LabelTypeMismatch
        );
    }

    #[test]
    fn metric_event_rejects_duplicate_labels() {
        let labels = vec![
            label(
                MetricLabelKey::Component,
                MetricLabelValue::Component(Component::Dns),
            ),
            label(
                MetricLabelKey::Component,
                MetricLabelValue::Component(Component::Cache),
            ),
        ];

        assert_eq!(
            MetricEvent::new(MetricName::RequestsTotal, labels, MetricValue::Counter(1),)
                .unwrap_err(),
            TelemetryFieldError::DuplicateLabel
        );
    }

    #[test]
    fn metric_event_rejects_labels_above_limit() {
        let registry = ConfiguredIdRegistry::from_validated([ValidatedConfiguredId::new(
            ConfiguredIdKind::Strategy,
            "primary",
        )
        .unwrap()]);
        let labels = vec![
            label(
                MetricLabelKey::Component,
                MetricLabelValue::Component(Component::Dns),
            ),
            label(
                MetricLabelKey::Transport,
                MetricLabelValue::Transport(TransportClass::Datagram),
            ),
            label(
                MetricLabelKey::Outcome,
                MetricLabelValue::Outcome(OutcomeClass::Success),
            ),
            label(
                MetricLabelKey::CacheStatus,
                MetricLabelValue::CacheStatus(CacheStatus::Fresh),
            ),
            label(
                MetricLabelKey::ConfiguredId,
                MetricLabelValue::ConfiguredId(
                    registry
                        .issue(ConfiguredIdKind::Strategy, "primary")
                        .unwrap(),
                ),
            ),
            label(
                MetricLabelKey::Component,
                MetricLabelValue::Component(Component::Cache),
            ),
        ];

        assert_eq!(labels.len(), MAX_METRIC_LABELS + 1);
        assert_eq!(
            MetricEvent::new(MetricName::RequestsTotal, labels, MetricValue::Counter(1),)
                .unwrap_err(),
            TelemetryFieldError::TooManyLabels
        );
    }

    #[test]
    fn metric_label_key_rejects_sensitive_and_high_cardinality_names() {
        assert_eq!(
            MetricLabelKey::parse("authorization"),
            Err(TelemetryFieldError::SensitiveKey)
        );
        assert_eq!(
            MetricLabelKey::parse("qname"),
            Err(TelemetryFieldError::SensitiveKey)
        );
        assert_eq!(
            MetricLabelKey::parse("tenant_free_form"),
            Err(TelemetryFieldError::UnknownLabel)
        );
    }

    #[test]
    fn valid_metric_event_exposes_only_validated_fields() {
        let event = MetricEvent::new(
            MetricName::RequestsTotal,
            vec![label(
                MetricLabelKey::Outcome,
                MetricLabelValue::Outcome(OutcomeClass::Success),
            )],
            MetricValue::Counter(1),
        )
        .unwrap();

        assert_eq!(event.name(), MetricName::RequestsTotal);
        assert_eq!(event.labels().len(), 1);
        assert_eq!(event.labels()[0].key(), MetricLabelKey::Outcome);
        assert!(matches!(event.value(), MetricValue::Counter(1)));
        assert_eq!(event.validate(), Ok(()));
    }

    #[test]
    fn configured_metric_labels_require_a_registered_token() {
        let registry = ConfiguredIdRegistry::from_validated([ValidatedConfiguredId::new(
            ConfiguredIdKind::Strategy,
            "primary",
        )
        .unwrap()]);

        for raw_request_value in ["private.example.test.", "client-7f3a9d"] {
            assert_eq!(
                registry.issue(ConfiguredIdKind::Strategy, raw_request_value),
                Err(ConfiguredIdError::UnknownConfigurationId)
            );
        }

        let configured = registry
            .issue(ConfiguredIdKind::Strategy, "primary")
            .unwrap();
        let label = MetricLabel::new(
            MetricLabelKey::ConfiguredId,
            MetricLabelValue::ConfiguredId(configured),
        )
        .unwrap();

        assert_eq!(label.key(), MetricLabelKey::ConfiguredId);
    }
}
