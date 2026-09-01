//! 已解析 upstream 的 connector 实现。

mod bootstrap;
mod doh;
mod executor;
mod group;
mod hosts;
mod http;
mod outcome;
mod registry;

pub use bootstrap::{
    AddressCachePolicy, AddressResolutionError, AddressResolutionRequest, AddressResolutionState,
    AddressSource, AnswerError, BootstrapAnswer, BootstrapResolution, CachePolicyError,
    DEFAULT_BOOTSTRAP_MAX_TTL, DEFAULT_BOOTSTRAP_MIN_TTL, NoAddressReason, ResolvedAddresses,
    SystemResolverAnswer, SystemResolverResolution,
};
pub use doh::{
    DOH_MEDIA_TYPE, DohEndpointError, DohExchange, DohHttpRequest, DohHttpResponse,
    DohHttpResponseOwned, DohHttpTransport, MAX_DOH_RESPONSE_BODY_BYTES, validate_response,
};
pub use executor::{ExecutorBuildError, ExecutorError, UpstreamGroupExecutor};
pub use group::{GroupSelector, GroupSelectorError, SelectionLease};
pub use hosts::{HostsExchange, HostsExchangeBuildError};
pub use http::TokioDohHttpTransport;
pub use outcome::{
    AttemptAssessment, AttemptClass, FallbackDecision, OutcomeError, UpstreamAttempt, aggregate,
    assess, deduplicate_connector_order,
};
pub use registry::{RegistryError, UpstreamRegistry};
