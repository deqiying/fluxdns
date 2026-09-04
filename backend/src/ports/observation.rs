//! 单次 DNS 解析完成事件与非阻塞发布契约。

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::cache::CacheCommitCandidate;
use crate::dns::{
    CancelReason, CanonicalQuestion, CanonicalResponse, RequestId, ResponseClass, RuntimeRevision,
    TransportClass,
};
use crate::resource::ResourceVersion;

use super::PortError;
use super::storage::{ResolveRuleSource, StatsSource};
use super::telemetry::{CacheStatus, OutcomeClass};

/// 只有启用 `resolve_log` 时才附带的请求级详情来源。
///
/// 该值保留 typed question 与共享 response；qname、answer 和 request digest 的字符串化
/// 必须由后台详情 projector 完成。
#[derive(Clone)]
pub struct ResolutionDetailSource {
    pub request_id: RequestId,
    pub client_ip: Option<IpAddr>,
    pub question: CanonicalQuestion,
    pub response: Option<Arc<CanonicalResponse>>,
}

impl fmt::Debug for ResolutionDetailSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolutionDetailSource")
            .field("has_request_id", &true)
            .field("has_client_ip", &self.client_ip.is_some())
            .field("question", &self.question)
            .field("has_response", &self.response.is_some())
            .finish()
    }
}

/// Core 完成时已经冻结的解析终态；不表达 transport encode/send 结果。
#[derive(Clone, Debug)]
pub enum ResolutionTerminal {
    Response { class: ResponseClass, rcode: u16 },
    NoResponse { reason: Option<CancelReason> },
    CoreFailure,
}

/// stats、详情和 health 共同消费的一次规范化解析完成事件。
#[derive(Clone)]
pub struct ResolutionEvent {
    pub occurred_at: SystemTime,
    pub duration_started_at: Instant,
    pub listener_id: Arc<str>,
    pub route_id: Option<Arc<str>>,
    pub client_bucket: Option<Arc<str>>,
    pub strategy_id: Option<Arc<str>>,
    pub upstream_id: Option<Arc<str>>,
    pub upstream_member_id: Option<Arc<str>>,
    pub upstream_used_id: Option<Arc<str>>,
    pub matched_rule_source: Option<ResolveRuleSource>,
    pub matched_resource_id: Option<Arc<str>>,
    pub matched_rule_ordinal: Option<u64>,
    pub resource_version: Option<ResourceVersion>,
    pub transport: TransportClass,
    pub terminal: ResolutionTerminal,
    pub outcome: OutcomeClass,
    pub source: StatsSource,
    /// 只描述客户端完成前已知的 lookup 状态；异步 write outcome 单独计数。
    pub cache_lookup_status: CacheStatus,
    pub runtime_revision: RuntimeRevision,
    pub detail: Option<ResolutionDetailSource>,
}

impl fmt::Debug for ResolutionEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolutionEvent")
            .field("occurred_at", &self.occurred_at)
            .field("duration_started_at", &self.duration_started_at)
            .field("listener_id", &self.listener_id)
            .field("has_route_id", &self.route_id.is_some())
            .field("has_client_bucket", &self.client_bucket.is_some())
            .field("has_strategy_id", &self.strategy_id.is_some())
            .field("has_upstream_id", &self.upstream_id.is_some())
            .field("has_upstream_member_id", &self.upstream_member_id.is_some())
            .field("has_upstream_used_id", &self.upstream_used_id.is_some())
            .field("matched_rule_source", &self.matched_rule_source)
            .field(
                "has_matched_resource_id",
                &self.matched_resource_id.is_some(),
            )
            .field("matched_rule_ordinal", &self.matched_rule_ordinal)
            .field("resource_version", &self.resource_version)
            .field("transport", &self.transport)
            .field("terminal", &self.terminal)
            .field("outcome", &self.outcome)
            .field("source", &self.source)
            .field("cache_lookup_status", &self.cache_lookup_status)
            .field("runtime_revision", &self.runtime_revision)
            .field("has_detail", &self.detail.is_some())
            .finish()
    }
}

/// 一次性发布单元；cache lease 不进入可 clone 的事件本体。
#[derive(Debug)]
pub struct ResolutionEnvelope {
    pub event: Arc<ResolutionEvent>,
    pub cache_commit: Option<CacheCommitCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionPublishDisposition {
    Accepted,
    DroppedQueueFull,
    Disabled,
}

/// DNS producer 唯一可见的事件入口；实现必须使用有界、无等待的发布方式。
pub trait ResolutionEventSink: Send + Sync {
    fn try_publish(
        &self,
        envelope: ResolutionEnvelope,
    ) -> Result<ResolutionPublishDisposition, PortError>;

    /// producer 据此决定是否附带详情 payload；该 interest 在进程启动时冻结。
    fn detail_enabled(&self) -> bool;
}
