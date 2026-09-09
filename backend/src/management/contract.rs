//! v2 HTTP/WS 公共契约；P0 不注册 handler，不持有运行态或文件写入能力。
#![allow(dead_code)] // P0 契约由测试消费；handler/owner 正式接线时移除此范围许可。

use serde::de::{self, DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};

use crate::config::contract::{ClientV2, DnsV2, StatisticsV2};
pub use crate::config::edit::{ConfigChange, ConfigModule};
use crate::config::model::{
    HostsResourceDto, ListenerDto, LogsDto, OutboundDto, RuleSetDto, StrategyDto, UpstreamDto,
};

pub const API_PREFIX: &str = "/api/v2";
pub const MAX_MUTATION_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_EXTERNAL_DIFF_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_CHANGES: usize = 128;
pub const MAX_CURSOR_BYTES: usize = 2048;
pub const MAX_QUERY_BYTES: usize = 16 * 1024;
pub const MAX_QUERY_PAGE_SIZE: u16 = 100;
pub const DEFAULT_QUERY_PAGE_SIZE: u16 = 20;
pub const MAX_WS_FRAME_BYTES: usize = 128 * 1024;
pub const WS_CONNECTION_CAPACITY: usize = 32;
pub const WS_CONNECTIONS_PER_SESSION: usize = 4;
pub const WS_SUBSCRIPTIONS_PER_CONNECTION: usize = 8;
pub const WS_QUEUE_BYTES: usize = 1024 * 1024;
pub const WS_QUEUE_MESSAGES: usize = 64;
pub const WS_HEARTBEAT_SECONDS: u64 = 15;
pub const WS_IDLE_SECONDS: u64 = 45;
pub const WS_WRITE_TIMEOUT_SECONDS: u64 = 5;
pub const WS_INBOUND_MESSAGES_PER_MINUTE: usize = 64;
pub const MAX_ONLINE_IDENTITIES: usize = 4_096;
pub const REPLAY_SECONDS: u64 = 60;
pub const REPLAY_RECORDS: usize = 5_000;
pub const REPLAY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_OPERATION_ENTRIES: usize = 1024;
pub const OPERATION_TTL_SECONDS: u64 = 1800;
pub const WS_PROTOCOL_VERSION: u16 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupState {
    Required,
    Ready,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupStatus {
    pub state: SetupState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub user: SessionUser,
    pub expires_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUser {
    pub name: String,
}

macro_rules! token {
    ($name:ident, $max:expr) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = &'static str;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                if value.is_empty()
                    || value.len() > $max
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-._:".contains(&byte))
                {
                    return Err("invalid bounded opaque token");
                }
                Ok(Self(value))
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::try_from(String::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }
    };
}

token!(Revision, 128);
token!(OperationId, 128);
token!(Cursor, MAX_CURSOR_BYTES);
token!(RecordId, 128);

/// 大计数按十进制 u64 字符串传输，拒绝溢出、前导零及科学计数法。
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct DecimalU64(String);

impl From<u64> for DecimalU64 {
    fn from(value: u64) -> Self {
        Self(value.to_string())
    }
}

impl DecimalU64 {
    pub fn as_u64(&self) -> u64 {
        self.0
            .parse()
            .expect("DecimalU64 is validated during construction")
    }
}

impl<'de> Deserialize<'de> for DecimalU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.parse::<u64>().is_err()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(de::Error::custom("invalid decimal u64"));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preconditions {
    pub active_revision: Revision,
    /// 源文件和派生副本的组合观测 token，包含缺失/不可读状态，不能仅绑定源文件。
    pub observed_file_revision: Revision,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileCondition {
    Unchanged,
    Changed,
    Missing,
    Unreadable,
    Oversized,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileObservation {
    pub source: FileCondition,
    pub derived: Option<FileCondition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncCondition {
    Synced,
    Applying,
    Persisting,
    AppliedUnpersisted,
    Blocked,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigState {
    pub active_revision: Revision,
    pub runtime_revision: Revision,
    pub persisted_revision: Option<Revision>,
    pub observed_file_revision: Revision,
    pub files: FileObservation,
    pub synchronization: SyncCondition,
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub expected: Preconditions,
    pub changes: Vec<ConfigChange>,
    pub discard_external_changes: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    pub operation_id: OperationId,
    pub candidate: Candidate,
    /// 与候选摘要、影响及双版本绑定；prepare 过期后必须重新验证。
    pub validation_token: Revision,
    pub confirmations: Vec<ImpactKind>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactKind {
    RenameReferences,
    ListenerRebind,
    RetentionShortening,
    DiscardExternalChanges,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationResult {
    pub validation_token: Revision,
    pub expected: Preconditions,
    pub expires_at_ms: u64,
    pub required_confirmations: Vec<ImpactKind>,
    pub affected_names: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSyncRequest {
    pub operation_id: OperationId,
    pub expected: Preconditions,
    pub discard_external_changes: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    HttpVersionNotSupported,
    UriTooLong,
    HeadersTooLarge,
    RequestTimeout,
    InternalError,
    SetupAlreadyCompleted,
    AuthInvalidCredentials,
    OriginRejected,
    ConfigConflict,
    InvalidArgument,
    ValidationFailed,
    PayloadTooLarge,
    VersionUnsupported,
    AuthRequired,
    Forbidden,
    NotFound,
    ActiveRevisionConflict,
    FileRevisionConflict,
    ExternalChangesRequireConfirmation,
    OperationIdReused,
    OperationBusy,
    ValidationExpired,
    PersistenceFailed,
    ApplyFailed,
    CompensationFailed,
    CursorExpired,
    RateLimited,
    ServiceUnavailable,
}

impl ErrorCode {
    /// 仅用于 ErrorEnvelope；已受理操作的失败状态仍由 OperationResult 返回。
    pub fn http_status(&self) -> u16 {
        match self {
            Self::HttpVersionNotSupported => 505,
            Self::UriTooLong => 414,
            Self::HeadersTooLarge => 431,
            Self::RequestTimeout => 408,
            Self::InternalError => 500,
            Self::SetupAlreadyCompleted => 409,
            Self::AuthInvalidCredentials => 401,
            Self::OriginRejected => 400,
            Self::ConfigConflict => 409,
            Self::InvalidArgument => 400,
            Self::ValidationFailed | Self::VersionUnsupported => 422,
            Self::PayloadTooLarge => 413,
            Self::AuthRequired => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::ActiveRevisionConflict
            | Self::FileRevisionConflict
            | Self::ExternalChangesRequireConfirmation
            | Self::OperationIdReused
            | Self::OperationBusy
            | Self::ValidationExpired => 409,
            Self::CursorExpired => 410,
            Self::RateLimited => 429,
            Self::PersistenceFailed | Self::ApplyFailed | Self::CompensationFailed => 500,
            Self::ServiceUnavailable => 503,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldError {
    pub path: String,
    pub code: ErrorCode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    pub code: ErrorCode,
    pub request_id: String,
    /// 仅允许安全的固定文案，不回显 YAML、凭据、SQL 或外部响应正文。
    pub message: String,
    pub retryable: bool,
    pub field_errors: Vec<FieldError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationStatus {
    Preparing {},
    Applying {},
    Persisting {
        active_revision: Revision,
    },
    AppliedSynced {
        active_revision: Revision,
        persisted_revision: Revision,
    },
    AppliedUnpersisted {
        active_revision: Revision,
        persisted_revision: Option<Revision>,
        error: ErrorCode,
    },
    Rejected {
        error: ErrorCode,
    },
    CompensationFailed {
        active_revision: Option<Revision>,
        error: ErrorCode,
    },
    /// 未找到、进程重启或记录过期均不能据此推断命令从未执行。
    Unknown {},
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationResult {
    pub operation_id: OperationId,
    pub status: OperationStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "module",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ModuleSource {
    Listener(ListenerDto),
    Upstreams(UpstreamDto),
    Strategy(StrategyDto),
    Hosts(HostsResourceDto),
    Outbound(OutboundDto),
    RuleSet(RuleSetDto),
    Clients(ClientV2),
    Dns(DnsV2),
    Statistics(StatisticsV2),
    Logs(LogsDto),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueOrigin {
    Default,
    Global,
    Strategy,
    Client,
    Explicit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScalarValue {
    Text(String),
    Boolean(bool),
    Integer(u64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveField {
    pub path: String,
    pub source: ValueOrigin,
    pub value: ScalarValue,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceReference {
    pub from_module: ConfigModule,
    pub from_name: String,
    pub path: String,
    pub to_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRead {
    pub state: ConfigState,
    pub values: Vec<ModuleSource>,
    pub effective: Vec<EffectiveField>,
    pub references: Vec<ResourceReference>,
    pub runtime: Vec<ModuleRuntime>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerBinding {
    pub endpoint_name: Option<String>,
    pub address: String,
    pub port: u16,
    pub transport: Transport,
    pub accepting: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceCondition {
    Ready,
    Stale,
    Failed,
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "module", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleRuntime {
    Listener {
        name: String,
        bindings: Vec<ListenerBinding>,
    },
    Hosts {
        name: String,
        condition: ResourceCondition,
        last_updated_at_ms: Option<u64>,
        next_update_at_ms: Option<u64>,
        error: Option<ErrorCode>,
    },
    RuleSet {
        name: String,
        condition: ResourceCondition,
        last_updated_at_ms: Option<u64>,
        next_update_at_ms: Option<u64>,
        error: Option<ErrorCode>,
    },
    Dns {
        snapshot: CacheSnapshotStatus,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCondition {
    Disabled,
    Idle,
    Writing,
    Restoring,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSnapshotStatus {
    pub state: SnapshotCondition,
    pub owner_revision: Revision,
    pub generation: DecimalU64,
    pub file_bytes: Option<DecimalU64>,
    pub last_success_at_ms: Option<u64>,
    pub last_error: Option<ErrorCode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupSection {
    Work,
    Database,
    Webui,
    ProtectedCredentials,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalResourceDiff {
    pub active: Option<ModuleSource>,
    pub external: Option<ModuleSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalDiff {
    pub expected: Preconditions,
    pub editable: Vec<ExternalResourceDiff>,
    pub protected_changes: Vec<StartupSection>,
    pub parse_error: Option<ErrorCode>,
}

/// 系统只读投影不包含 users、hash、私钥或实际 Secret 值。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemConfigRead {
    pub state: ConfigState,
    pub work_path: String,
    pub rules_path: String,
    pub database_path: String,
    pub records_path: String,
    pub webui_enabled: bool,
    pub webui_address: String,
    pub webui_port: u16,
    pub public_origin: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryFilter {
    pub from_ms: u64,
    pub to_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qtype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rcode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<QuerySource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<QueryOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheOutcome>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageDirection {
    Older,
    Newer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    pub filter: QueryFilter,
    pub cursor: Option<Cursor>,
    pub direction: PageDirection,
    pub page_size: u16,
    pub sort: QuerySort,
    pub order: SortOrder,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuerySort {
    OccurredAt,
    Duration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuerySource {
    Cache,
    Hosts,
    Rule,
    Upstream,
    Synthetic,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryOutcome {
    Answered,
    Negative,
    Timeout,
    Rejected,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheOutcome {
    Hit,
    Stale,
    Miss,
    Bypass,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Udp,
    Tcp,
    Doh,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestIdentity {
    pub client_id: Option<String>,
    pub client_ip: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoricalMatch {
    Id { matched_client_id: String },
    Ip { matched_client_id: String },
    None {},
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsAnswer {
    pub name: String,
    pub r#type: String,
    pub ttl_seconds: u32,
    pub data: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnswerSummary {
    Available {
        records: Vec<DnsAnswer>,
        total_count: u32,
    },
    Truncated {
        records: Vec<DnsAnswer>,
        total_count: u32,
    },
    Unavailable {},
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRecord {
    pub id: RecordId,
    pub occurred_at_ms: u64,
    pub identity: RequestIdentity,
    pub matched: HistoricalMatch,
    pub current_client_name: Option<String>,
    pub qname: String,
    pub qtype: String,
    pub transport: Transport,
    pub rcode: String,
    pub source: QuerySource,
    pub outcome: QueryOutcome,
    pub cache: CacheOutcome,
    pub strategy_name: Option<String>,
    pub upstream_target_name: Option<String>,
    pub upstream_used_name: Option<String>,
    pub cache_producer: Option<CacheProducer>,
    pub duration_us: Option<u64>,
    pub dns_core_duration_us: Option<u64>,
    pub answers: AnswerSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheProducer {
    pub strategy_name: Option<String>,
    pub upstream_target_name: Option<String>,
    pub upstream_used_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitCursor {
    pub epoch: Revision,
    pub sequence: DecimalU64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryPage {
    pub items: Vec<QueryRecord>,
    pub previous_cursor: Option<Cursor>,
    pub next_cursor: Option<Cursor>,
    pub snapshot_cursor: CommitCursor,
    pub directory_revision: Revision,
    pub retention_revision: Revision,
    pub available_from_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryDetail {
    pub record: QueryRecord,
    pub directory_revision: Revision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    Warmup,
    ObservationGap,
    SamplingFailed,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Measurement<T> {
    Available {
        value: T,
    },
    Unavailable {
        reason: UnavailableReason,
        /// 暖机时返回当前已覆盖秒数；其他缺数原因固定为 null。
        observed_seconds: Option<u64>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateSample {
    pub at_ms: u64,
    pub value: Measurement<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceMetrics {
    pub sampled_at_ms: u64,
    pub qps: Measurement<f64>,
    pub rpm: Measurement<f64>,
    pub online_clients: Measurement<u32>,
    pub rss_bytes: Measurement<DecimalU64>,
    pub qps_trend: Vec<RateSample>,
    pub rpm_trend: Vec<RateSample>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessMetrics {
    pub version: String,
    pub started_at_ms: u64,
    pub sampled_at_ms: u64,
    pub uptime_seconds: u64,
    pub rss_bytes: Measurement<DecimalU64>,
    pub cpu_percent: Measurement<f64>,
    pub threads: Measurement<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSocketTicket {
    pub ticket: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionStatus {
    pub policy: StatisticsV2,
    pub sampled_at_ms: u64,
    pub detail_bytes: DecimalU64,
    pub cutoff_utc_date: Option<String>,
    pub last_completed_at_ms: Option<u64>,
    pub next_scheduled_at_ms: Option<u64>,
    pub pending_reclaim_bytes: DecimalU64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionPreview {
    pub expected: Preconditions,
    pub sampled_at_ms: u64,
    pub detail_bytes: DecimalU64,
    pub proposed_cutoff_utc_date: String,
    pub shortens_history: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionPreviewRequest {
    pub expected: Preconditions,
    pub policy: StatisticsV2,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    SubscribeMetrics {
        subscription_id: Revision,
    },
    SubscribeQueries {
        subscription_id: Revision,
        filter: Box<QueryFilter>,
        after: CommitCursor,
        retention_revision: Revision,
    },
    Unsubscribe {
        subscription_id: Revision,
    },
    Pong {
        nonce: Revision,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    EpochChanged,
    CursorExpired,
    BufferOverflow,
    ObservationGap,
    RetentionChanged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    Ready {
        protocol_version: u16,
        epoch: Revision,
    },
    Metrics {
        subscription_id: Revision,
        data: ServiceMetrics,
    },
    Queries {
        subscription_id: Revision,
        cursor: CommitCursor,
        directory_revision: Revision,
        items: Vec<QueryRecord>,
    },
    ResyncRequired {
        subscription_id: Revision,
        reason: ResyncReason,
    },
    ConfigChanged {
        active_revision: Revision,
        observed_file_revision: Revision,
    },
    Ping {
        nonce: Revision,
    },
}

/// 供后续 handler 共用的有界 JSON 解码入口；完整候选引用/影响检查由 ConfigStore 执行。
pub fn decode_candidate(bytes: &[u8]) -> Result<Candidate, ErrorCode> {
    let tree: serde_json::Value = decode_json(bytes, MAX_MUTATION_BYTES)?;
    if contains_null(&tree) {
        return Err(ErrorCode::InvalidArgument);
    }
    let value: Candidate = serde_json::from_value(tree).map_err(|_| ErrorCode::InvalidArgument)?;
    if value.changes.is_empty() || value.changes.len() > MAX_CHANGES {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(value)
}

/// 普通模块表单不能借通用候选封套修改其他模块；组合采用只走专门的整体入口。
pub fn decode_module_candidate(bytes: &[u8], module: ConfigModule) -> Result<Candidate, ErrorCode> {
    let value = decode_candidate(bytes)?;
    if value.changes.len() != 1 || value.changes[0].module() != module {
        return Err(ErrorCode::Forbidden);
    }
    Ok(value)
}

/// 应用封套沿用候选预算，不在此执行 prepare、文件覆盖或幂等记录。
pub fn decode_apply(bytes: &[u8], module: Option<ConfigModule>) -> Result<ApplyRequest, ErrorCode> {
    let tree: serde_json::Value = decode_json(bytes, MAX_MUTATION_BYTES)?;
    if contains_null(&tree) {
        return Err(ErrorCode::InvalidArgument);
    }
    let request: ApplyRequest =
        serde_json::from_value(tree).map_err(|_| ErrorCode::InvalidArgument)?;
    if request.candidate.changes.is_empty()
        || request.candidate.changes.len() > MAX_CHANGES
        || request.confirmations.len() > 4
    {
        return Err(ErrorCode::InvalidArgument);
    }
    if let Some(module) = module
        && (request.candidate.changes.len() != 1 || request.candidate.changes[0].module() != module)
    {
        return Err(ErrorCode::Forbidden);
    }
    if request
        .confirmations
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != request.confirmations.len()
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(request)
}

/// 文件动作只接受固定 operation、双版本和覆盖确认，不接收路径或配置正文。
pub fn decode_file_sync(bytes: &[u8]) -> Result<FileSyncRequest, ErrorCode> {
    let tree: serde_json::Value = decode_json(bytes, MAX_MUTATION_BYTES)?;
    if contains_null(&tree) {
        return Err(ErrorCode::InvalidArgument);
    }
    serde_json::from_value(tree).map_err(|_| ErrorCode::InvalidArgument)
}

fn contains_null(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Array(values) => values.iter().any(contains_null),
        serde_json::Value::Object(values) => values.values().any(contains_null),
        _ => false,
    }
}

/// 查询预算独立于业务保留配额；cursor 的 filter/hash/水位校验在 storage 读口完成。
pub fn decode_query(bytes: &[u8]) -> Result<QueryRequest, ErrorCode> {
    let tree: serde_json::Value = decode_json(bytes, MAX_QUERY_BYTES)?;
    if tree.get("filter").is_some_and(contains_null) {
        return Err(ErrorCode::InvalidArgument);
    }
    let query: QueryRequest =
        serde_json::from_value(tree).map_err(|_| ErrorCode::InvalidArgument)?;
    if query.page_size == 0 || query.page_size > MAX_QUERY_PAGE_SIZE {
        return Err(ErrorCode::InvalidArgument);
    }
    validate_query_filter(&query.filter)?;
    Ok(query)
}

/// WS 与 REST 共用筛选约束，避免相同查询从订阅入口绕过预算。
pub fn decode_client_message(bytes: &[u8]) -> Result<ClientMessage, ErrorCode> {
    let tree: serde_json::Value = decode_json(bytes, MAX_WS_FRAME_BYTES)?;
    if contains_null(&tree) {
        return Err(ErrorCode::InvalidArgument);
    }
    let message: ClientMessage =
        serde_json::from_value(tree).map_err(|_| ErrorCode::InvalidArgument)?;
    if let ClientMessage::SubscribeQueries {
        filter,
        retention_revision,
        ..
    } = &message
    {
        validate_query_filter(filter)?;
        retention_revision
            .as_str()
            .parse::<u64>()
            .map_err(|_| ErrorCode::InvalidArgument)?;
    }
    Ok(message)
}

fn validate_query_filter(filter: &QueryFilter) -> Result<(), ErrorCode> {
    if filter.to_ms > 9_007_199_254_740_991
        || filter.from_ms >= filter.to_ms
        || filter.to_ms - filter.from_ms > 3650 * 86_400_000
        || filter
            .client_id
            .as_ref()
            .is_some_and(|id| !crate::config::contract::valid_client_id(id))
        || filter
            .matched_client_id
            .as_ref()
            .is_some_and(|id| !crate::config::contract::valid_client_id(id))
        || filter
            .client_ip
            .as_ref()
            .is_some_and(|ip| ip.parse::<std::net::IpAddr>().is_err())
        || filter.client_name.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > 128 || name.chars().any(char::is_control)
        })
        || filter.qname.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > 253 || name.chars().any(char::is_control)
        })
        || filter
            .qtype
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 16)
        || filter
            .rcode
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 16)
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}

pub fn decode_json<T: DeserializeOwned>(bytes: &[u8], limit: usize) -> Result<T, ErrorCode> {
    if bytes.len() > limit {
        return Err(ErrorCode::PayloadTooLarge);
    }
    serde_json::from_slice(bytes).map_err(|_| ErrorCode::InvalidArgument)
}

#[cfg(test)]
mod tests;
