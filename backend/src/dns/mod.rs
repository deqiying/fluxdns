//! Transport 无关的 DNS 核心契约。

mod configured;
mod context;
mod handler;
mod hosts;
mod message;
mod policy;

pub use configured::{ConfiguredDnsCore, CoreBuildError, DEFAULT_LOCAL_TTL};
pub use context::{
    CacheCompatibilityKey, CancelReason, Cancellation, ClientId, ClientIdentity, ConnectionId,
    Deadline, DnsRequest, ListenerId, RequestContext, RequestId, RequestMeta, RouteId,
    RuntimeRevision, StreamId, TraceId, TransportCapabilities, TransportClass,
};
pub use handler::{
    CoreError, CoreOutcome, DispatchError, DispatchOutcome, DnsCore, HostsCore, ServFailCore,
    dispatch_inbound,
};
pub use hosts::{HostsParseError, HostsTable};
pub use message::{
    CanonicalMessageError, CanonicalQuery, CanonicalQuestion, CanonicalResponse, DnsMessageId,
    ResponseClass, TtlMetadata,
};
pub use policy::{PolicyCoreBuildError, PolicyDnsCore, PolicyResourcePublishError};
pub(crate) use policy::{RuntimeCoreCell, RuntimeCoreTarget};
