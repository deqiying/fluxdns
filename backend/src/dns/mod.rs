//! Transport 无关的 DNS 核心契约。

mod context;
mod handler;
mod message;

pub use context::{
    CacheCompatibilityKey, CancelReason, Cancellation, ClientId, ClientIdentity, ConnectionId,
    Deadline, DnsRequest, ListenerId, RequestContext, RequestId, RequestMeta, RouteId,
    RuntimeRevision, StreamId, TraceId, TransportCapabilities, TransportClass,
};
pub use handler::{
    CoreError, CoreOutcome, DispatchError, DispatchOutcome, DnsCore, ServFailCore, dispatch_inbound,
};
pub use message::{
    CanonicalMessageError, CanonicalQuery, CanonicalQuestion, CanonicalResponse, DnsMessageId,
    ResponseClass, TtlMetadata,
};
