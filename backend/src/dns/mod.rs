//! Transport 无关的 DNS 核心契约。

mod context;
mod handler;
mod hosts;
mod message;
mod policy;

pub use context::{
    CacheCompatibilityKey, CancelReason, Cancellation, ClientId, ClientIdentity, ConnectionId,
    Deadline, DnsRequest, ListenerId, RequestContext, RequestId, RequestMeta, RouteId,
    RuntimeRevision, StreamId, TraceId, TransportCapabilities, TransportClass,
};
pub use handler::{
    CoreError, CoreOutcome, DispatchError, DispatchOutcome, DnsCore, DnsCoreCompletion,
    DnsResolutionObservation, HostsCore, MatchedRuleObservation, MatchedRuleSource, ServFailCore,
    dispatch_inbound,
};
pub use hosts::{HostsParseError, HostsTable};
pub use message::{
    CanonicalMessageError, CanonicalQuery, CanonicalQuestion, CanonicalResponse, DnsMessageId,
    ResponseClass, TtlMetadata,
};
pub use policy::{PolicyCoreBuildError, PolicyDnsCore, PolicyResourcePublishError};
pub(crate) use policy::{RuntimeCoreCell, RuntimeCoreTarget};

/// 本地 hosts 响应与 hosts upstream 共用的默认 TTL，单位为秒。
pub const DEFAULT_LOCAL_TTL: u32 = 60;
