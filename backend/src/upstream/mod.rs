//! 已解析 upstream 的 connector 实现。

mod bootstrap;
mod doh;
mod executor;
mod group;
mod hosts;
mod http;
mod outbound;
mod outcome;
mod registry;
mod reqwest_http;
mod socks5;
mod tokio_outbound;

pub(crate) use bootstrap::BootstrapConnectorRegistry;
pub use bootstrap::{
    AddressCachePolicy, AddressResolutionError, AddressResolutionRequest, AddressResolutionState,
    AddressSource, AnswerError, BootstrapAnswer, BootstrapResolution, BootstrapResolver,
    BootstrapResolverError, BootstrapResponseError, CachePolicyError, DEFAULT_BOOTSTRAP_MAX_TTL,
    DEFAULT_BOOTSTRAP_MIN_TTL, NoAddressReason, ResolvedAddresses, SystemResolverAnswer,
    SystemResolverResolution, bootstrap_answer_from_response,
};
pub use doh::{
    DOH_MEDIA_TYPE, DohEndpointError, DohExchange, DohHttpRequest, DohHttpResponse,
    DohHttpResponseOwned, DohHttpTransport, MAX_DOH_RESPONSE_BODY_BYTES, validate_response,
};
pub use executor::{ExecutorBuildError, ExecutorError, UpstreamGroupExecutor};
pub use group::{GroupSelector, GroupSelectorError, SelectionLease};
pub use hosts::{HostsExchange, HostsExchangeBuildError};
pub use http::{
    DohAddressRequest, DohAddressResolver, TokioDohAddressResolver, TokioDohHttpTransport,
    TokioSocks5DohHttpTransport,
};
pub use outbound::{
    NameResolution, OutboundCredentials, OutboundProfile, OutboundProfileError, OutboundTarget,
    OutboundTargetError, Socks5ConnectError, Socks5Connector,
};
pub use outcome::{
    AttemptAssessment, AttemptClass, FallbackDecision, OutcomeError, UpstreamAttempt, aggregate,
    assess, deduplicate_connector_order, should_enter_fallback,
};
pub use registry::{RegistryError, UpstreamRegistry};
pub use reqwest_http::{ReqwestDohHttpTransport, ReqwestDohHttpTransportBuildError};
pub use socks5::{
    Socks5Address, Socks5AuthMethod, Socks5ConnectResponse, Socks5Credentials,
    Socks5HandshakeError, Socks5ProtocolError, Socks5Reply, Socks5TargetError, address_for_target,
    encode_connect_request, encode_method_request, encode_userpass_request, parse_connect_response,
    parse_method_response, parse_userpass_response, perform_handshake,
};
pub use tokio_outbound::{TokioOutboundAddressResolver, TokioOutboundDialer};
