//! DoH HTTP request/response codec and endpoint assembly.
//!
//! The system socket port owns TLS byte-stream wrapping; this module owns the
//! endpoint's TLS material selection, HTTP envelope, and client address policy.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use thiserror::Error;
use tokio::sync::Mutex;

use ipnet::IpNet;

use crate::config::model::{ClientIpSource, ForwardedDisposition, ForwardedHeader, TlsMode};
use crate::config::resolve::ResolvedListener;
use crate::config::{BindProtocol, BindTransport, DohBindingRef, ResolvedConfig};
use crate::dns::{
    CancelReason, Cancellation, ClientId, ClientIdentity, ConnectionId, Deadline, DnsRequest,
    ListenerId, RequestContext, RequestId, RequestMeta, RouteId, RuntimeRevision, StreamId,
    TransportCapabilities, TransportClass,
};
use crate::ports::effects::{
    ActivatedSocketHandle, TcpConnectionHandle, TcpListenerHandle, TcpReadChunkResult,
    TlsServerMaterial,
};
use crate::ports::inbound::{InboundRequest, ResponseEncoder};
use crate::ports::{PortError, PortErrorClass, PortFuture};
use crate::runtime::BoundEndpointHandle;

use super::wire::{MAX_DNS_WIRE_BYTES, ParsedQuery, WireError, decode_query};

pub const MAX_DOH_POST_BODY_BYTES: usize = MAX_DNS_WIRE_BYTES;
pub const MAX_DOH_GET_DNS_CHARS: usize = 87_380;
pub const MAX_DOH_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_DOH_REQUEST_TARGET_BYTES: usize = 131_072;
pub const MAX_PROXY_V1_BYTES: usize = 107;
pub const MAX_PROXY_V2_BYTES: usize = 536;

const MAX_HEADER_COUNT: usize = 64;
const DOH_READ_CHUNK_BYTES: usize = 8 * 1024;
// request-line 与 header fields 分开计费，确保完整 GET wire 不会提前撞上 header 上限。
const MAX_DOH_REQUEST_LINE_BYTES: usize = MAX_DOH_REQUEST_TARGET_BYTES + 32;
const MAX_DOH_REQUEST_HEAD_BYTES: usize = MAX_DOH_REQUEST_LINE_BYTES + MAX_DOH_HEADER_BYTES;
const MAX_DOH_BUFFER_BYTES: usize = MAX_DOH_REQUEST_HEAD_BYTES + MAX_DOH_POST_BODY_BYTES;
const PROXY_V2_SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";
const PROXY_V1_PREFIX: &[u8; 6] = b"PROXY ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DohHttpMethod {
    Get,
    Post,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDohRequest {
    pub method: DohHttpMethod,
    pub path: String,
    pub query: ParsedQuery,
    pub wire: Vec<u8>,
    pub connection_close: bool,
    pub consumed_bytes: usize,
    pub(crate) forwarded_headers: ParsedForwardedHeaders,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ParsedForwardedHeaders {
    pub x_forwarded_for: Option<String>,
    pub x_real_ip: Option<String>,
    pub forwarded: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DohClientIpPolicy {
    source: ClientIpSource,
    header: Option<ForwardedHeader>,
    trusted_proxies: Vec<IpNet>,
    on_missing: ForwardedDisposition,
    on_invalid: ForwardedDisposition,
}

impl Default for DohClientIpPolicy {
    fn default() -> Self {
        Self {
            source: ClientIpSource::Peer,
            header: None,
            trusted_proxies: Vec::new(),
            on_missing: ForwardedDisposition::Reject,
            on_invalid: ForwardedDisposition::Reject,
        }
    }
}

impl DohClientIpPolicy {
    fn from_resolved(client_ip: &crate::config::resolve::ResolvedClientIp) -> Self {
        Self {
            source: client_ip.source,
            header: client_ip.header,
            trusted_proxies: client_ip.trusted_proxies.clone().unwrap_or_default(),
            on_missing: client_ip.on_missing.unwrap_or(ForwardedDisposition::Reject),
            on_invalid: client_ip.on_invalid.unwrap_or(ForwardedDisposition::Reject),
        }
    }

    fn resolve(
        &self,
        peer: SocketAddr,
        headers: &ParsedForwardedHeaders,
        proxy_client: Option<IpAddr>,
    ) -> Result<std::net::IpAddr, ClientIpResolutionError> {
        if self.source == ClientIpSource::Peer {
            return Ok(peer.ip());
        }
        if !self
            .trusted_proxies
            .iter()
            .any(|network| network.contains(&peer.ip()))
        {
            return Err(ClientIpResolutionError::UntrustedPeer);
        }
        if self.source == ClientIpSource::ProxyProtocol {
            return proxy_client.ok_or(ClientIpResolutionError::Invalid);
        }
        let Some(header) = self.header else {
            return Err(ClientIpResolutionError::Missing);
        };
        let value = match header {
            ForwardedHeader::XForwardedFor => headers.x_forwarded_for.as_deref(),
            ForwardedHeader::XRealIp => headers.x_real_ip.as_deref(),
            ForwardedHeader::Forwarded => headers.forwarded.as_deref(),
        };
        let Some(value) = value else {
            return match self.on_missing {
                ForwardedDisposition::Reject => Err(ClientIpResolutionError::Missing),
                ForwardedDisposition::UsePeer => Ok(peer.ip()),
            };
        };
        let chain = match header {
            ForwardedHeader::XForwardedFor => parse_ip_chain(value),
            ForwardedHeader::XRealIp => parse_single_ip(value).map(|ip| vec![ip]),
            ForwardedHeader::Forwarded => parse_forwarded_chain(value),
        };
        let Ok(chain) = chain else {
            return match self.on_invalid {
                ForwardedDisposition::Reject => Err(ClientIpResolutionError::Invalid),
                ForwardedDisposition::UsePeer => Ok(peer.ip()),
            };
        };
        select_forwarded_client(&chain, &self.trusted_proxies)
            .ok_or(ClientIpResolutionError::Invalid)
    }

    fn peer_is_trusted(&self, peer: IpAddr) -> bool {
        self.source == ClientIpSource::Peer
            || self
                .trusted_proxies
                .iter()
                .any(|network| network.contains(&peer))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientIpResolutionError {
    UntrustedPeer,
    Missing,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProxyHeaderParse {
    Incomplete,
    Complete { consumed: usize, client: IpAddr },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DohRoutePattern {
    template: String,
    strategy: String,
    placeholder: Option<(usize, usize)>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct DohRouteMatch {
    pub template: String,
    pub strategy: String,
    pub client_id: Option<ClientId>,
}

impl fmt::Debug for DohRouteMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DohRouteMatch")
            .field("template", &self.template)
            .field("strategy", &self.strategy)
            .field(
                "has_client_id",
                &self
                    .client_id
                    .as_ref()
                    .is_some_and(|id| !id.as_str().is_empty()),
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum DohRouteError {
    #[error("DoH route path must be a non-empty absolute path")]
    InvalidPath,
    #[error("DoH route path contains an invalid client_id placeholder")]
    InvalidPlaceholder,
    #[error("DoH route strategy must not be empty")]
    EmptyStrategy,
}

impl DohRoutePattern {
    pub fn new(
        template: impl Into<String>,
        strategy: impl Into<String>,
    ) -> Result<Self, DohRouteError> {
        let template = template.into();
        let strategy = strategy.into();
        if template.is_empty()
            || !template.starts_with('/')
            || template.contains('?')
            || template.contains('#')
            || template
                .as_bytes()
                .iter()
                .any(|byte| *byte < 0x20 || *byte == 0x7f)
        {
            return Err(DohRouteError::InvalidPath);
        }
        if strategy.trim().is_empty() {
            return Err(DohRouteError::EmptyStrategy);
        }

        let marker = "{client_id}";
        let first = template.find(marker);
        if first.is_some_and(|index| template[index + marker.len()..].contains(marker)) {
            return Err(DohRouteError::InvalidPlaceholder);
        }
        let placeholder = first.map(|start| (start, start + marker.len()));
        if let Some((start, end)) = placeholder {
            let is_segment_start = start == 0 || template.as_bytes()[start - 1] == b'/';
            let is_segment_end = end == template.len() || template.as_bytes()[end] == b'/';
            if !is_segment_start || !is_segment_end {
                return Err(DohRouteError::InvalidPlaceholder);
            }
        }

        Ok(Self {
            template,
            strategy,
            placeholder,
        })
    }

    pub fn template(&self) -> &str {
        &self.template
    }

    pub fn strategy(&self) -> &str {
        &self.strategy
    }

    pub fn matches(&self, path: &str) -> Option<DohRouteMatch> {
        let client_id = match self.placeholder {
            None if path == self.template => None,
            Some((start, end)) => {
                if !path.starts_with(&self.template[..start])
                    || !path.ends_with(&self.template[end..])
                {
                    return None;
                }
                let value_end = path.len().checked_sub(self.template.len() - end)?;
                let value = &path[start..value_end];
                if value.is_empty() || value.contains('/') || value.contains('?') {
                    return None;
                }
                Some(ClientId::new(value.to_owned()))
            }
            _ => return None,
        };
        Some(DohRouteMatch {
            template: self.template.clone(),
            strategy: self.strategy.clone(),
            client_id,
        })
    }
}

#[derive(Debug, Error)]
pub enum DohAdapterError {
    #[error("DoH adapter requires a positive request timeout")]
    InvalidTimeout,
    #[error("DoH endpoint and activated socket protocol do not match")]
    ProtocolMismatch,
    #[error("DoH adapter requires multiplexed transport capabilities")]
    InvalidTransportClass,
    #[error("DoH bind entry is missing typed endpoint metadata")]
    MissingBinding,
    #[error("DoH listener `{listener}` was not found in resolved configuration")]
    ListenerNotFound { listener: String },
    #[error("DoH endpoint `{endpoint}` was not found in resolved configuration")]
    EndpointNotFound { endpoint: String },
    #[error("DoH TLS certificate could not be loaded")]
    TlsCertificateLoad,
    #[error("DoH TLS private key could not be loaded")]
    TlsPrivateKeyLoad,
    #[error("DoH TLS material is invalid")]
    TlsInvalidMaterial,
    #[error("DoH route is invalid: {0}")]
    InvalidRoute(#[from] DohRouteError),
}

fn load_tls_material(
    certificate_file: Option<&Path>,
    private_key_file: Option<&Path>,
) -> Result<TlsServerMaterial, DohAdapterError> {
    let certificate_file = certificate_file.ok_or(DohAdapterError::TlsInvalidMaterial)?;
    let private_key_file = private_key_file.ok_or(DohAdapterError::TlsInvalidMaterial)?;
    let certificate_bytes =
        std::fs::read(certificate_file).map_err(|_| DohAdapterError::TlsCertificateLoad)?;
    let mut certificate_chain = CertificateDer::pem_slice_iter(&certificate_bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| DohAdapterError::TlsCertificateLoad)?
        .into_iter()
        .map(|certificate| certificate.as_ref().to_vec())
        .collect::<Vec<_>>();
    if certificate_chain.is_empty() && !certificate_bytes.is_empty() {
        certificate_chain.push(certificate_bytes);
    }
    if certificate_chain.is_empty() {
        return Err(DohAdapterError::TlsCertificateLoad);
    }

    let private_key_bytes =
        std::fs::read(private_key_file).map_err(|_| DohAdapterError::TlsPrivateKeyLoad)?;
    let private_key = match PrivateKeyDer::from_pem_slice(&private_key_bytes) {
        Ok(key) => key.secret_der().to_vec(),
        Err(_) => PrivateKeyDer::try_from(private_key_bytes)
            .map_err(|_| DohAdapterError::TlsPrivateKeyLoad)?
            .secret_der()
            .to_vec(),
    };
    if private_key.is_empty() {
        return Err(DohAdapterError::TlsPrivateKeyLoad);
    }
    let material = TlsServerMaterial {
        certificate_chain,
        private_key,
    };
    validate_tls_material(&material)?;
    Ok(material)
}

fn validate_tls_material(material: &TlsServerMaterial) -> Result<(), DohAdapterError> {
    let certificates = material
        .certificate_chain
        .iter()
        .cloned()
        .map(CertificateDer::from)
        .collect::<Vec<_>>();
    let private_key = PrivateKeyDer::try_from(material.private_key.clone())
        .map_err(|_| DohAdapterError::TlsInvalidMaterial)?;
    let provider = rustls::crypto::ring::default_provider();
    ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|_| DohAdapterError::TlsInvalidMaterial)?
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|_| DohAdapterError::TlsInvalidMaterial)?;
    Ok(())
}

/// HTTP-level outcome produced by one DoH session read.
pub enum DohSessionEvent {
    Request(InboundRequest),
    HttpError { error: DohHttpError, close: bool },
    CleanEof,
}

/// DoH listener adapter. The underlying socket remains a TCP capability;
/// `BindTransport::Doh` selects this application protocol.
#[derive(Clone)]
pub struct DohAdapter {
    listener: Arc<dyn TcpListenerHandle>,
    binding: DohBindingRef,
    routes: Arc<Vec<DohRoutePattern>>,
    runtime_revision: RuntimeRevision,
    transport: TransportCapabilities,
    request_ids: Arc<AtomicU64>,
    connection_ids: Arc<AtomicU64>,
    request_timeout: Duration,
    client_ip: DohClientIpPolicy,
    tls_material: Option<Arc<TlsServerMaterial>>,
}

/// One ordered HTTP/1.x connection. Requests are processed serially in v1;
/// the connection may stay open for another request after a valid response.
pub struct DohSession {
    connection: Arc<Mutex<Box<dyn TcpConnectionHandle>>>,
    peer: SocketAddr,
    connection_id: ConnectionId,
    next_stream_id: u64,
    request_ids: Arc<AtomicU64>,
    binding: DohBindingRef,
    listener_id: ListenerId,
    routes: Arc<Vec<DohRoutePattern>>,
    runtime_revision: RuntimeRevision,
    transport: TransportCapabilities,
    request_timeout: Duration,
    client_ip: DohClientIpPolicy,
    tls_material: Option<Arc<TlsServerMaterial>>,
    tls_started: bool,
    proxy_client: Option<IpAddr>,
    proxy_header_done: bool,
    read_buffer: Vec<u8>,
    pending_close: bool,
}

impl fmt::Debug for DohAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DohAdapter")
            .field("binding", &self.binding)
            .field("route_count", &self.routes.len())
            .field("runtime_revision", &self.runtime_revision)
            .field("transport", &self.transport)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl DohAdapter {
    pub fn from_endpoint(
        endpoint: BoundEndpointHandle,
        config: &ResolvedConfig,
        runtime_revision: RuntimeRevision,
        transport: TransportCapabilities,
        request_timeout: Duration,
    ) -> Result<Self, DohAdapterError> {
        if endpoint.entry.protocol != BindProtocol::Tcp
            || endpoint.entry.transport != BindTransport::Doh
        {
            return Err(DohAdapterError::ProtocolMismatch);
        }
        let binding = endpoint
            .entry
            .doh_binding
            .clone()
            .ok_or(DohAdapterError::MissingBinding)?;
        let ActivatedSocketHandle::Tcp(listener) = endpoint.socket else {
            return Err(DohAdapterError::ProtocolMismatch);
        };

        let Some(ResolvedListener::Doh {
            id: _,
            routes,
            endpoints,
        }) = config
            .listeners
            .iter()
            .find(|listener| matches!(listener, ResolvedListener::Doh { id, .. } if id.as_str() == binding.listener_id))
        else {
            return Err(DohAdapterError::ListenerNotFound {
                listener: binding.listener_id,
            });
        };
        let endpoint_config = endpoints
            .iter()
            .find(|candidate| {
                candidate.binding == binding
                    && candidate.port == endpoint.entry.port
                    && candidate.addresses.contains(&endpoint.entry.address)
            })
            .ok_or_else(|| DohAdapterError::EndpointNotFound {
                endpoint: binding.endpoint_id.clone(),
            })?;
        let tls_material = match endpoint_config.tls_mode {
            TlsMode::External => None,
            TlsMode::Terminate => Some(Arc::new(load_tls_material(
                endpoint_config.certificate_file.as_deref(),
                endpoint_config.private_key_file.as_deref(),
            )?)),
        };
        let route_patterns = routes
            .iter()
            .map(|route| DohRoutePattern::new(route.path.clone(), route.strategy.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        Self::new_with_tls(
            listener,
            binding,
            route_patterns,
            runtime_revision,
            transport,
            request_timeout,
            DohClientIpPolicy::from_resolved(&endpoint_config.client_ip),
            tls_material,
        )
    }

    pub fn new(
        listener: Arc<dyn TcpListenerHandle>,
        binding: DohBindingRef,
        routes: Vec<DohRoutePattern>,
        runtime_revision: RuntimeRevision,
        transport: TransportCapabilities,
        request_timeout: Duration,
    ) -> Result<Self, DohAdapterError> {
        Self::new_with_policy(
            listener,
            binding,
            routes,
            runtime_revision,
            transport,
            request_timeout,
            DohClientIpPolicy::default(),
        )
    }

    fn new_with_policy(
        listener: Arc<dyn TcpListenerHandle>,
        binding: DohBindingRef,
        routes: Vec<DohRoutePattern>,
        runtime_revision: RuntimeRevision,
        transport: TransportCapabilities,
        request_timeout: Duration,
        client_ip: DohClientIpPolicy,
    ) -> Result<Self, DohAdapterError> {
        Self::new_with_tls(
            listener,
            binding,
            routes,
            runtime_revision,
            transport,
            request_timeout,
            client_ip,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_tls(
        listener: Arc<dyn TcpListenerHandle>,
        binding: DohBindingRef,
        routes: Vec<DohRoutePattern>,
        runtime_revision: RuntimeRevision,
        transport: TransportCapabilities,
        request_timeout: Duration,
        client_ip: DohClientIpPolicy,
        tls_material: Option<Arc<TlsServerMaterial>>,
    ) -> Result<Self, DohAdapterError> {
        if request_timeout.is_zero() {
            return Err(DohAdapterError::InvalidTimeout);
        }
        if transport.class != TransportClass::Multiplexed {
            return Err(DohAdapterError::InvalidTransportClass);
        }
        if routes.is_empty() {
            return Err(DohAdapterError::InvalidRoute(DohRouteError::InvalidPath));
        }
        Ok(Self {
            listener,
            binding,
            routes: Arc::new(routes),
            runtime_revision,
            transport,
            request_ids: Arc::new(AtomicU64::new(0)),
            connection_ids: Arc::new(AtomicU64::new(0)),
            request_timeout,
            client_ip,
            tls_material,
        })
    }

    pub fn binding(&self) -> &DohBindingRef {
        &self.binding
    }

    pub fn accept_session<'a>(
        &'a self,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<DohSession>, PortError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            let deadline = Deadline::new(Instant::now() + self.request_timeout);
            let connection = self.listener.accept(deadline, cancellation).await?;
            let Some(connection) = connection else {
                return Ok(None);
            };
            let connection = Arc::new(Mutex::new(connection));
            let peer = {
                let connection = connection.lock().await;
                connection.peer_addr()?
            };
            let connection_id = ConnectionId::from(
                self.connection_ids
                    .fetch_add(1, Ordering::AcqRel)
                    .wrapping_add(1),
            );
            Ok(Some(DohSession {
                connection,
                peer,
                connection_id,
                next_stream_id: 0,
                request_ids: Arc::clone(&self.request_ids),
                binding: self.binding.clone(),
                listener_id: ListenerId::from(self.binding.listener_id.clone()),
                routes: Arc::clone(&self.routes),
                runtime_revision: self.runtime_revision,
                transport: self.transport,
                request_timeout: self.request_timeout,
                client_ip: self.client_ip.clone(),
                tls_material: self.tls_material.clone(),
                tls_started: false,
                proxy_client: None,
                proxy_header_done: self.client_ip.source != ClientIpSource::ProxyProtocol,
                read_buffer: Vec::new(),
                pending_close: false,
            }))
        })
    }
}

impl DohSession {
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn binding(&self) -> &DohBindingRef {
        &self.binding
    }

    pub fn response_should_close(&self) -> bool {
        self.pending_close
    }

    async fn read_next_chunk(
        &self,
        max_bytes: usize,
        deadline: Deadline,
        cancellation: &Cancellation,
    ) -> Result<TcpReadChunkResult, PortError> {
        let mut connection = self.connection.lock().await;
        connection
            .read_chunk(max_bytes, deadline, cancellation)
            .await
    }

    pub fn receive<'a>(
        &'a mut self,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohSessionEvent, PortError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(cancelled_error("transport.doh.receive"));
            }
            let received_at = Instant::now();
            let deadline = Deadline::new(received_at + self.request_timeout);
            loop {
                if !self.client_ip.peer_is_trusted(self.peer.ip()) {
                    self.read_buffer.clear();
                    self.pending_close = true;
                    return Ok(DohSessionEvent::HttpError {
                        error: DohHttpError::Malformed,
                        close: true,
                    });
                }
                if !self.tls_started
                    && self.proxy_header_done
                    && let Some(material) = &self.tls_material
                {
                    let mut connection = self.connection.lock().await;
                    connection
                        .start_tls(Arc::clone(material), deadline, cancellation)
                        .await?;
                    self.tls_started = true;
                    continue;
                }
                if !self.proxy_header_done {
                    match parse_proxy_header(&self.read_buffer) {
                        Ok(ProxyHeaderParse::Complete { consumed, client }) => {
                            self.read_buffer.drain(..consumed);
                            self.proxy_client = Some(client);
                            self.proxy_header_done = true;
                            continue;
                        }
                        Ok(ProxyHeaderParse::Incomplete) => {
                            if self.read_buffer.len() >= MAX_PROXY_V2_BYTES {
                                self.read_buffer.clear();
                                self.pending_close = true;
                                return Ok(DohSessionEvent::HttpError {
                                    error: DohHttpError::Malformed,
                                    close: true,
                                });
                            }
                            match self.read_next_chunk(1, deadline, cancellation).await? {
                                TcpReadChunkResult::Data(bytes)
                                    if !bytes.is_empty()
                                        && bytes.len() <= DOH_READ_CHUNK_BYTES
                                        && self.read_buffer.len() + bytes.len()
                                            <= MAX_PROXY_V2_BYTES =>
                                {
                                    self.read_buffer.extend_from_slice(&bytes);
                                }
                                TcpReadChunkResult::Data(_) | TcpReadChunkResult::CleanEof => {
                                    self.read_buffer.clear();
                                    self.pending_close = true;
                                    return Ok(DohSessionEvent::HttpError {
                                        error: DohHttpError::Malformed,
                                        close: true,
                                    });
                                }
                            }
                            continue;
                        }
                        Err(()) => {
                            self.read_buffer.clear();
                            self.pending_close = true;
                            return Ok(DohSessionEvent::HttpError {
                                error: DohHttpError::Malformed,
                                close: true,
                            });
                        }
                    }
                }
                match try_parse_request(&self.read_buffer) {
                    Ok(Some(parsed)) => {
                        self.read_buffer.drain(..parsed.consumed_bytes);
                        let route = self
                            .routes
                            .iter()
                            .find_map(|route| route.matches(&parsed.path));
                        let Some(route) = route else {
                            self.pending_close = parsed.connection_close;
                            return Ok(DohSessionEvent::HttpError {
                                error: DohHttpError::NotFound,
                                close: parsed.connection_close,
                            });
                        };
                        let client_addr = match self.client_ip.resolve(
                            self.peer,
                            &parsed.forwarded_headers,
                            self.proxy_client,
                        ) {
                            Ok(client_addr) => client_addr,
                            Err(ClientIpResolutionError::UntrustedPeer)
                            | Err(ClientIpResolutionError::Missing)
                            | Err(ClientIpResolutionError::Invalid) => {
                                self.pending_close = true;
                                return Ok(DohSessionEvent::HttpError {
                                    error: DohHttpError::Malformed,
                                    close: true,
                                });
                            }
                        };
                        self.pending_close = parsed.connection_close;
                        self.next_stream_id = self.next_stream_id.wrapping_add(1).max(1);
                        let request_id = RequestId::from(
                            self.request_ids
                                .fetch_add(1, Ordering::AcqRel)
                                .wrapping_add(1) as u128,
                        );
                        let request_cancellation = Cancellation::new();
                        let context = RequestContext {
                            meta: RequestMeta {
                                request_id,
                                trace_id: None,
                                received_at,
                                received_at_utc: SystemTime::now(),
                                deadline,
                                cancellation: request_cancellation,
                                connection_id: Some(self.connection_id),
                                stream_id: Some(StreamId::from(self.next_stream_id)),
                                listener_id: self.listener_id.clone(),
                                route_id: Some(RouteId::from(route.template.clone())),
                                original_dns_id: Some(parsed.query.id.value()),
                            },
                            client: ClientIdentity {
                                peer_addr: Some(self.peer),
                                client_addr: Some(client_addr),
                                client_id: route.client_id,
                            },
                            transport: self.transport,
                            runtime_revision: self.runtime_revision,
                        };
                        let encoder = Arc::new(DohResponseEncoder {
                            connection: Arc::clone(&self.connection),
                            close: parsed.connection_close,
                        });
                        return Ok(DohSessionEvent::Request(InboundRequest::new(
                            DnsRequest {
                                query: parsed.query.query,
                                context,
                            },
                            encoder,
                        )));
                    }
                    Ok(None) => {
                        if self.read_buffer.len() >= MAX_DOH_BUFFER_BYTES {
                            self.read_buffer.clear();
                            self.pending_close = true;
                            return Ok(DohSessionEvent::HttpError {
                                error: DohHttpError::PayloadTooLarge,
                                close: true,
                            });
                        }
                        let result = self
                            .read_next_chunk(DOH_READ_CHUNK_BYTES, deadline, cancellation)
                            .await?;
                        match result {
                            TcpReadChunkResult::Data(bytes) => {
                                if bytes.is_empty()
                                    || bytes.len() > DOH_READ_CHUNK_BYTES
                                    || self.read_buffer.len() + bytes.len() > MAX_DOH_BUFFER_BYTES
                                {
                                    self.read_buffer.clear();
                                    self.pending_close = true;
                                    return Ok(DohSessionEvent::HttpError {
                                        error: DohHttpError::PayloadTooLarge,
                                        close: true,
                                    });
                                }
                                self.read_buffer.extend_from_slice(&bytes);
                            }
                            TcpReadChunkResult::CleanEof => {
                                if self.read_buffer.is_empty() {
                                    return Ok(DohSessionEvent::CleanEof);
                                }
                                return Err(PortError::new(
                                    PortErrorClass::ProtocolViolation,
                                    "transport.doh.eof",
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        self.read_buffer.clear();
                        self.pending_close = true;
                        return Ok(DohSessionEvent::HttpError { error, close: true });
                    }
                }
            }
        })
    }

    pub fn write_http_error<'a>(
        &'a self,
        error: DohHttpError,
        close: bool,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        let payload = encode_http_error_with_close(error, close || error.should_close());
        let deadline = Deadline::new(Instant::now() + self.request_timeout);
        Box::pin(async move {
            let mut connection = self.connection.lock().await;
            connection.write_all(payload, deadline, cancellation).await
        })
    }

    pub async fn close(&mut self) {
        let mut connection = self.connection.lock().await;
        let _ = connection.shutdown().await;
    }
}

struct DohResponseEncoder {
    connection: Arc<Mutex<Box<dyn TcpConnectionHandle>>>,
    close: bool,
}

impl ResponseEncoder for DohResponseEncoder {
    fn encode<'a>(
        &'a self,
        request: &'a DnsRequest,
        response: crate::dns::CanonicalResponse,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let id = request
                .context
                .meta
                .original_dns_id
                .map(crate::dns::DnsMessageId::new)
                .ok_or_else(|| {
                    PortError::new(PortErrorClass::ProtocolViolation, "transport.doh.encode")
                })?;
            let dns = super::wire::encode_response(&response, id, MAX_DNS_WIRE_BYTES)
                .map_err(map_doh_wire_error)?;
            let payload = encode_dns_response(&dns, self.close);
            let mut connection = self.connection.lock().await;
            connection
                .write_all(
                    payload,
                    request.context.meta.deadline,
                    &request.context.meta.cancellation,
                )
                .await
        })
    }
}

fn map_doh_wire_error(error: super::wire::WireError) -> PortError {
    let class = match error {
        super::wire::WireError::TooLarge { .. } => PortErrorClass::ResourceExhausted,
        super::wire::WireError::Empty
        | super::wire::WireError::Decode
        | super::wire::WireError::InvalidQuery(_) => PortErrorClass::ProtocolViolation,
        super::wire::WireError::Encode => PortErrorClass::Internal,
    };
    PortError::new(class, "transport.doh.encode")
}

fn cancelled_error(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Cancelled(CancelReason::Shutdown), operation)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DohHttpStatus {
    Ok,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    PayloadTooLarge,
    UriTooLong,
    UnsupportedMediaType,
    InternalServerError,
}

impl DohHttpStatus {
    pub const fn code(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::BadRequest => 400,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::PayloadTooLarge => 413,
            Self::UriTooLong => 414,
            Self::UnsupportedMediaType => 415,
            Self::InternalServerError => 500,
        }
    }

    const fn reason(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::BadRequest => "Bad Request",
            Self::NotFound => "Not Found",
            Self::MethodNotAllowed => "Method Not Allowed",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::UriTooLong => "URI Too Long",
            Self::UnsupportedMediaType => "Unsupported Media Type",
            Self::InternalServerError => "Internal Server Error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum DohHttpError {
    #[error("HTTP request is incomplete")]
    Incomplete,
    #[error("HTTP request is malformed")]
    Malformed,
    #[error("HTTP method is not supported")]
    MethodNotAllowed,
    #[error("DoH route was not found")]
    NotFound,
    #[error("GET request is missing the dns parameter")]
    MissingDnsParameter,
    #[error("GET request contains duplicate dns parameters")]
    DuplicateDnsParameter,
    #[error("GET dns parameter is not valid unpadded base64url")]
    InvalidDnsParameter,
    #[error("DoH request target is too long")]
    UriTooLong,
    #[error("DoH POST body is too large")]
    PayloadTooLarge,
    #[error("DoH POST content type is unsupported")]
    UnsupportedMediaType,
    #[error("DNS wire message is invalid")]
    InvalidDnsWire,
    #[error("transfer encoding is unsupported")]
    UnsupportedTransferEncoding,
    #[error("POST request requires a content length")]
    MissingContentLength,
    #[error("HTTP Host header is missing, duplicated, or invalid")]
    InvalidHost,
}

impl DohHttpError {
    pub const fn status(self) -> DohHttpStatus {
        match self {
            Self::UriTooLong => DohHttpStatus::UriTooLong,
            Self::PayloadTooLarge => DohHttpStatus::PayloadTooLarge,
            Self::UnsupportedMediaType => DohHttpStatus::UnsupportedMediaType,
            Self::MethodNotAllowed => DohHttpStatus::MethodNotAllowed,
            Self::NotFound => DohHttpStatus::NotFound,
            Self::Incomplete
            | Self::Malformed
            | Self::MissingDnsParameter
            | Self::DuplicateDnsParameter
            | Self::InvalidDnsParameter
            | Self::InvalidDnsWire
            | Self::UnsupportedTransferEncoding
            | Self::MissingContentLength
            | Self::InvalidHost => DohHttpStatus::BadRequest,
        }
    }

    pub const fn should_close(self) -> bool {
        matches!(
            self,
            Self::Incomplete
                | Self::Malformed
                | Self::UriTooLong
                | Self::PayloadTooLarge
                | Self::UnsupportedTransferEncoding
                | Self::InvalidHost
        )
    }
}

/// 从 `buffer` 开头解析一条完整 HTTP 请求。
///
/// `Ok(None)` 表示仍需读取更多字节。返回错误时调用方写入对应 HTTP 状态；
/// `should_close()` 为 true 时关闭当前连接。
pub fn try_parse_request(buffer: &[u8]) -> Result<Option<ParsedDohRequest>, DohHttpError> {
    let Some(header_end) = find_subslice(buffer, b"\r\n\r\n") else {
        if buffer.len() > MAX_DOH_REQUEST_HEAD_BYTES {
            return Err(DohHttpError::Malformed);
        }
        return Ok(None);
    };
    if header_end > MAX_DOH_REQUEST_HEAD_BYTES {
        return Err(DohHttpError::Malformed);
    }

    let header = &buffer[..header_end];
    if header.iter().any(|byte| *byte >= 0x80) {
        return Err(DohHttpError::Malformed);
    }
    let header = std::str::from_utf8(header).map_err(|_| DohHttpError::Malformed)?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or(DohHttpError::Malformed)?;
    let header_fields_bytes = header_end
        .checked_sub(request_line.len())
        .ok_or(DohHttpError::Malformed)?
        .saturating_sub(2);
    if header_fields_bytes > MAX_DOH_HEADER_BYTES {
        return Err(DohHttpError::Malformed);
    }
    let parts = request_line.split(' ').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(DohHttpError::Malformed);
    }
    // 严格校验 method token 和可见 ASCII request-target，避免上下游采用不同宽松规则。
    if !parts[0].bytes().all(is_token_byte) || !parts[1].bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(DohHttpError::Malformed);
    }
    let method = match parts[0] {
        "GET" => DohHttpMethod::Get,
        "POST" => DohHttpMethod::Post,
        _ => return Err(DohHttpError::MethodNotAllowed),
    };
    if parts[2] != "HTTP/1.1" && parts[2] != "HTTP/1.0" {
        return Err(DohHttpError::Malformed);
    }
    if parts[1].len() > MAX_DOH_REQUEST_TARGET_BYTES {
        return Err(DohHttpError::UriTooLong);
    }
    let target = parts[1];

    let mut content_length = None;
    let mut content_type = None;
    let mut host_present = false;
    let mut connection_close = parts[2] == "HTTP/1.0";
    let mut forwarded_headers = ParsedForwardedHeaders::default();
    let mut header_count = 0_usize;
    for line in lines {
        if line.is_empty() {
            return Err(DohHttpError::Malformed);
        }
        header_count += 1;
        if header_count > MAX_HEADER_COUNT {
            return Err(DohHttpError::Malformed);
        }
        let separator = line.find(':').ok_or(DohHttpError::Malformed)?;
        let (name, value) = line.split_at(separator);
        let value = &value[1..];
        if name.is_empty() || !name.as_bytes().iter().copied().all(is_token_byte) {
            return Err(DohHttpError::Malformed);
        }
        if value
            .as_bytes()
            .iter()
            .any(|byte| *byte < 0x20 && *byte != b'\t' || *byte == 0x7f)
        {
            return Err(DohHttpError::Malformed);
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                if content_length.is_some() {
                    return Err(DohHttpError::Malformed);
                }
                let parsed = parse_content_length(value)?;
                content_length = Some(parsed);
            }
            "content-type" => {
                if content_type.replace(value.to_owned()).is_some() {
                    return Err(DohHttpError::Malformed);
                }
            }
            "transfer-encoding" => return Err(DohHttpError::UnsupportedTransferEncoding),
            "host" => {
                if host_present || !is_valid_host_header(value) {
                    return Err(DohHttpError::InvalidHost);
                }
                host_present = true;
            }
            "connection"
                if value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("close")) =>
            {
                connection_close = true;
            }
            "x-forwarded-for" => {
                if forwarded_headers
                    .x_forwarded_for
                    .replace(value.to_owned())
                    .is_some()
                {
                    return Err(DohHttpError::Malformed);
                }
            }
            "x-real-ip" => {
                if forwarded_headers
                    .x_real_ip
                    .replace(value.to_owned())
                    .is_some()
                {
                    return Err(DohHttpError::Malformed);
                }
            }
            "forwarded"
                if forwarded_headers
                    .forwarded
                    .replace(value.to_owned())
                    .is_some() =>
            {
                return Err(DohHttpError::Malformed);
            }
            "forwarded" => {}
            _ => {}
        }
    }

    // HTTP/1.1 缺少 Host 或任何 HTTP/1.x 请求重复 Host 都按 400 关闭，避免 authority 歧义。
    if parts[2] == "HTTP/1.1" && !host_present {
        return Err(DohHttpError::InvalidHost);
    }

    let body_length = content_length.unwrap_or(0);
    if body_length > MAX_DOH_POST_BODY_BYTES {
        return Err(DohHttpError::PayloadTooLarge);
    }
    let body_start = header_end + 4;
    let body_end = body_start
        .checked_add(body_length)
        .ok_or(DohHttpError::PayloadTooLarge)?;
    if buffer.len() < body_end {
        return Ok(None);
    }
    let body = &buffer[body_start..body_end];
    let (path, query_string) = split_target(target)?;

    let wire = match method {
        DohHttpMethod::Get => {
            if body_length != 0 {
                return Err(DohHttpError::Malformed);
            }
            let dns_value = get_dns_parameter(query_string)?;
            if dns_value.len() > MAX_DOH_GET_DNS_CHARS {
                return Err(DohHttpError::UriTooLong);
            }
            let dns_value =
                std::str::from_utf8(&dns_value).map_err(|_| DohHttpError::InvalidDnsParameter)?;
            decode_base64url(dns_value).map_err(|_| DohHttpError::InvalidDnsParameter)?
        }
        DohHttpMethod::Post => {
            if content_length.is_none() {
                return Err(DohHttpError::MissingContentLength);
            }
            if !content_type
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("application/dns-message"))
            {
                return Err(DohHttpError::UnsupportedMediaType);
            }
            if body.is_empty() {
                return Err(DohHttpError::InvalidDnsWire);
            }
            body.to_vec()
        }
    };
    let query = decode_query(&wire, MAX_DNS_WIRE_BYTES).map_err(|error| match error {
        WireError::TooLarge { .. } => DohHttpError::PayloadTooLarge,
        _ => DohHttpError::InvalidDnsWire,
    })?;

    Ok(Some(ParsedDohRequest {
        method,
        path: path.to_owned(),
        query,
        wire,
        connection_close,
        consumed_bytes: body_end,
        forwarded_headers,
    }))
}

fn parse_single_ip(value: &str) -> Result<IpAddr, ()> {
    let value = value.trim();
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(());
    }
    value.parse().map_err(|_| ())
}

fn parse_proxy_header(buffer: &[u8]) -> Result<ProxyHeaderParse, ()> {
    if PROXY_V1_PREFIX.starts_with(buffer) {
        return Ok(ProxyHeaderParse::Incomplete);
    }
    if buffer.starts_with(PROXY_V1_PREFIX) {
        let Some(line_end) = find_subslice(buffer, b"\r\n") else {
            return if buffer.len() < MAX_PROXY_V1_BYTES {
                Ok(ProxyHeaderParse::Incomplete)
            } else {
                Err(())
            };
        };
        let consumed = line_end + 2;
        if consumed > MAX_PROXY_V1_BYTES {
            return Err(());
        }
        let line = std::str::from_utf8(&buffer[..line_end]).map_err(|_| ())?;
        let mut parts = line.split_ascii_whitespace();
        if parts.next() != Some("PROXY") {
            return Err(());
        }
        let protocol = parts.next().ok_or(())?;
        if !matches!(protocol, "TCP4" | "TCP6") || parts.clone().count() != 4 {
            return Err(());
        }
        let source = parse_single_ip(parts.next().ok_or(())?)?;
        let destination = parse_single_ip(parts.next().ok_or(())?)?;
        let source_port = parse_proxy_port(parts.next().ok_or(())?)?;
        let destination_port = parse_proxy_port(parts.next().ok_or(())?)?;
        if (protocol == "TCP4" && (!source.is_ipv4() || !destination.is_ipv4()))
            || (protocol == "TCP6" && (!source.is_ipv6() || !destination.is_ipv6()))
            || source_port == 0
            || destination_port == 0
        {
            return Err(());
        }
        return Ok(ProxyHeaderParse::Complete {
            consumed,
            client: source,
        });
    }

    if PROXY_V2_SIGNATURE.starts_with(buffer) {
        return Ok(ProxyHeaderParse::Incomplete);
    }
    if !buffer.starts_with(PROXY_V2_SIGNATURE) {
        return Err(());
    }
    if buffer.len() < 16 {
        return Ok(ProxyHeaderParse::Incomplete);
    }
    let version_command = buffer[12];
    if version_command >> 4 != 0x2 || version_command & 0x0f != 0x1 {
        return Err(());
    }
    let family_protocol = buffer[13];
    let payload_length = u16::from_be_bytes([buffer[14], buffer[15]]) as usize;
    let consumed = 16_usize.checked_add(payload_length).ok_or(())?;
    if consumed > MAX_PROXY_V2_BYTES {
        return Err(());
    }
    if buffer.len() < consumed {
        return Ok(ProxyHeaderParse::Incomplete);
    }
    let (source, source_port, destination_port) = match family_protocol {
        0x11 if payload_length >= 12 => {
            let source = IpAddr::V4(std::net::Ipv4Addr::new(
                buffer[16], buffer[17], buffer[18], buffer[19],
            ));
            let port = u16::from_be_bytes([buffer[24], buffer[25]]);
            let destination_port = u16::from_be_bytes([buffer[26], buffer[27]]);
            (source, port, destination_port)
        }
        0x21 if payload_length >= 36 => {
            let mut octets = [0_u8; 16];
            octets.copy_from_slice(&buffer[16..32]);
            let source = IpAddr::V6(std::net::Ipv6Addr::from(octets));
            let port = u16::from_be_bytes([buffer[48], buffer[49]]);
            let destination_port = u16::from_be_bytes([buffer[50], buffer[51]]);
            (source, port, destination_port)
        }
        _ => return Err(()),
    };
    if source_port == 0 || destination_port == 0 {
        return Err(());
    }
    Ok(ProxyHeaderParse::Complete {
        consumed,
        client: source,
    })
}

fn parse_proxy_port(value: &str) -> Result<u16, ()> {
    value.parse::<u16>().map_err(|_| ())
}

fn parse_ip_chain(value: &str) -> Result<Vec<IpAddr>, ()> {
    let chain = value
        .split(',')
        .map(parse_single_ip)
        .collect::<Result<Vec<_>, _>>()?;
    if chain.is_empty() {
        return Err(());
    }
    Ok(chain)
}

fn parse_forwarded_chain(value: &str) -> Result<Vec<IpAddr>, ()> {
    let mut chain = Vec::new();
    for element in value.split(',') {
        let mut address = None;
        for parameter in element.split(';') {
            let (name, value) = parameter.trim().split_once('=').ok_or(())?;
            if name.trim().eq_ignore_ascii_case("for") {
                if address.is_some() {
                    return Err(());
                }
                address = Some(parse_forwarded_ip(value.trim())?);
            }
        }
        chain.push(address.ok_or(())?);
    }
    if chain.is_empty() {
        return Err(());
    }
    Ok(chain)
}

fn parse_forwarded_ip(value: &str) -> Result<IpAddr, ()> {
    let value = if value.starts_with('"') {
        if !value.ends_with('"') || value.len() < 2 {
            return Err(());
        }
        let value = &value[1..value.len() - 1];
        if value.contains('"') || value.contains('\\') {
            return Err(());
        }
        value
    } else {
        value
    };
    if let Some(value) = value.strip_prefix('[') {
        let end = value.find(']').ok_or(())?;
        let address = &value[..end];
        let suffix = &value[end + 1..];
        if !suffix.is_empty() && (!suffix.starts_with(':') || suffix[1..].parse::<u16>().is_err()) {
            return Err(());
        }
        return address.parse().map_err(|_| ());
    }
    parse_single_ip(value)
}

fn select_forwarded_client(chain: &[IpAddr], trusted_proxies: &[IpNet]) -> Option<IpAddr> {
    chain
        .iter()
        .rev()
        .find(|address| {
            !trusted_proxies
                .iter()
                .any(|network| network.contains(*address))
        })
        .copied()
        .or_else(|| chain.first().copied())
}

pub fn encode_http_response(
    status: DohHttpStatus,
    body: &[u8],
    content_type: Option<&str>,
    close: bool,
) -> Vec<u8> {
    encode_http_response_with_allow(status, body, content_type, close, None)
}

pub fn encode_http_error(error: DohHttpError) -> Vec<u8> {
    encode_http_error_with_close(error, error.should_close())
}

pub fn encode_http_error_with_close(error: DohHttpError, close: bool) -> Vec<u8> {
    let allow = (error == DohHttpError::MethodNotAllowed).then_some("GET, POST");
    encode_http_response_with_allow(error.status(), &[], None, close, allow)
}

pub fn encode_dns_response(body: &[u8], close: bool) -> Vec<u8> {
    encode_http_response_with_allow(
        DohHttpStatus::Ok,
        body,
        Some("application/dns-message"),
        close,
        None,
    )
}

fn encode_http_response_with_allow(
    status: DohHttpStatus,
    body: &[u8],
    content_type: Option<&str>,
    close: bool,
    allow: Option<&str>,
) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {} {}\r\n", status.code(), status.reason());
    if let Some(allow) = allow {
        response.push_str("Allow: ");
        response.push_str(allow);
        response.push_str("\r\n");
    }
    if let Some(content_type) = content_type {
        response.push_str("Content-Type: ");
        response.push_str(content_type);
        response.push_str("\r\n");
    }
    response.push_str("Cache-Control: no-store\r\n");
    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    if close {
        response.push_str("Connection: close\r\n");
    }
    response.push_str("\r\n");
    let mut bytes = response.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn split_target(target: &str) -> Result<(&str, &str), DohHttpError> {
    if !target.starts_with('/') || target.contains('#') {
        return Err(DohHttpError::Malformed);
    }
    match target.split_once('?') {
        Some((path, query)) if !path.is_empty() => Ok((path, query)),
        Some(_) => Err(DohHttpError::Malformed),
        None => Ok((target, "")),
    }
}

fn get_dns_parameter(query: &str) -> Result<Vec<u8>, DohHttpError> {
    let mut result = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or(DohHttpError::Malformed)?;
        let key = percent_decode(key).map_err(|_| DohHttpError::Malformed)?;
        if key != b"dns" {
            continue;
        }
        if result.is_some() {
            return Err(DohHttpError::DuplicateDnsParameter);
        }
        result = Some(percent_decode(value).map_err(|_| DohHttpError::InvalidDnsParameter)?);
    }
    result.ok_or(DohHttpError::MissingDnsParameter)
}

fn percent_decode(value: &str) -> Result<Vec<u8>, ()> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(());
            }
            let high = hex_value(bytes[index + 1]).ok_or(())?;
            let low = hex_value(bytes[index + 2]).ok_or(())?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    Ok(output)
}

fn decode_base64url(value: &str) -> Result<Vec<u8>, ()> {
    if value.is_empty() || value.contains('=') || value.len() % 4 == 1 {
        return Err(());
    }
    let mut output = Vec::with_capacity(value.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in value.bytes() {
        let sextet = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(()),
        } as u32;
        accumulator = (accumulator << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1_u32 << bits).saturating_sub(1);
            if output.len() > MAX_DNS_WIRE_BYTES {
                return Err(());
            }
        }
    }
    if bits > 0 && accumulator != 0 {
        return Err(());
    }
    Ok(output)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 严格解析十进制 Content-Length，拒绝整数 parser 可能接受的符号前缀。
fn parse_content_length(value: &str) -> Result<usize, DohHttpError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DohHttpError::Malformed);
    }
    value.parse().map_err(|_| DohHttpError::Malformed)
}

/// 校验 Host 为不含 userinfo、路径或查询参数的 HTTP authority。
fn is_valid_host_header(value: &str) -> bool {
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return false;
    }
    let Ok(url) = url::Url::parse(&format!("http://{value}/")) else {
        return false;
    };
    url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};
    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    use crate::dns::{
        CacheCompatibilityKey, Cancellation, RuntimeRevision, ServFailCore, TransportCapabilities,
        TransportClass, dispatch_inbound,
    };
    use crate::ports::effects::{
        ActivatedSocketHandle, SocketFactory, SocketKind, SocketSpec, TcpConnectionHandle,
        TcpListenerHandle, TcpReadChunkResult, TcpReadResult, TlsServerMaterial,
    };
    use crate::ports::{PortError, PortErrorClass, PortFuture};
    use crate::runtime::SystemSocketFactory;

    use super::*;

    fn wire() -> Vec<u8> {
        let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("Example.COM.").unwrap(),
            RecordType::A,
        ));
        message.to_vec().unwrap()
    }

    fn base64url(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut output = String::new();
        let mut index = 0;
        while index < bytes.len() {
            let first = bytes[index];
            let second = bytes.get(index + 1).copied();
            let third = bytes.get(index + 2).copied();
            output.push(TABLE[(first >> 2) as usize] as char);
            output.push(TABLE[((first & 0x03) << 4 | second.unwrap_or(0) >> 4) as usize] as char);
            if let Some(second) = second {
                output
                    .push(TABLE[((second & 0x0f) << 2 | third.unwrap_or(0) >> 6) as usize] as char);
            }
            if let Some(third) = third {
                output.push(TABLE[(third & 0x3f) as usize] as char);
            }
            index += 3;
        }
        output
    }

    fn request(method: &str, target: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes =
            format!("{method} {target} HTTP/1.1\r\nHost: doh.test\r\n{headers}\r\n").into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn loads_pem_tls_material_and_rejects_mismatched_key() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let other = rcgen::generate_simple_self_signed(vec!["otherhost".to_owned()]).unwrap();
        let root = std::env::temp_dir().join(format!(
            "fluxdns-doh-tls-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let certificate_path = root.join("cert.pem");
        let key_path = root.join("key.pem");
        fs::write(&certificate_path, certified.cert.pem()).unwrap();
        fs::write(&key_path, certified.signing_key.serialize_pem()).unwrap();
        let material = load_tls_material(Some(&certificate_path), Some(&key_path)).unwrap();
        assert_eq!(material.certificate_chain.len(), 1);
        assert!(!material.private_key.is_empty());

        fs::write(&key_path, other.signing_key.serialize_pem()).unwrap();
        assert!(matches!(
            load_tls_material(Some(&certificate_path), Some(&key_path)),
            Err(DohAdapterError::TlsInvalidMaterial)
        ));
        let _ = fs::remove_dir_all(root);
    }

    /// 验证无需 PEM 包装的 DER 证书和私钥可直接装配为 TLS material。
    #[test]
    fn loads_der_tls_material() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let certificate_der = certified.cert.der().to_vec();
        let private_key_der = certified.signing_key.serialize_der();
        let root = std::env::temp_dir().join(format!(
            "fluxdns-doh-tls-der-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let certificate_path = root.join("cert.der");
        let key_path = root.join("key.der");
        fs::write(&certificate_path, &certificate_der).unwrap();
        fs::write(&key_path, &private_key_der).unwrap();

        let material = load_tls_material(Some(&certificate_path), Some(&key_path)).unwrap();
        assert_eq!(material.certificate_chain, vec![certificate_der]);
        assert_eq!(material.private_key, private_key_der);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn session_consumes_proxy_header_before_upgrading_tls() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let certificate_der = certified.cert.der().to_vec();
        let material = Arc::new(TlsServerMaterial {
            certificate_chain: vec![certificate_der.clone()],
            private_key: certified.signing_key.serialize_der(),
        });
        let factory = SystemSocketFactory::new();
        let cancellation = Cancellation::new();
        let prepared = factory
            .prepare(
                SocketSpec {
                    kind: SocketKind::Tcp,
                    address: "127.0.0.1:0".parse().unwrap(),
                    reuse_port: false,
                    v6_only: false,
                },
                Deadline::new(std::time::Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        let activated = prepared.activate().unwrap();
        let ActivatedSocketHandle::Tcp(listener) = activated.socket_handle().unwrap() else {
            unreachable!();
        };
        let address = listener.local_addr().unwrap();
        let adapter = DohAdapter::new_with_tls(
            Arc::clone(&listener),
            crate::config::DohBindingRef {
                listener_id: "doh".to_owned(),
                endpoint_id: "tls".to_owned(),
            },
            vec![DohRoutePattern::new("/dns", "default").unwrap()],
            RuntimeRevision(1),
            doh_capabilities(),
            Duration::from_secs(3),
            DohClientIpPolicy {
                source: ClientIpSource::ProxyProtocol,
                header: None,
                trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
                on_missing: ForwardedDisposition::Reject,
                on_invalid: ForwardedDisposition::Reject,
            },
            Some(material),
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let cancellation = Cancellation::new();
            let mut session = adapter
                .accept_session(&cancellation)
                .await
                .unwrap()
                .unwrap();
            session.receive(&cancellation).await.unwrap()
        });

        let client = TcpStream::connect(address).await.unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(certificate_der)).unwrap();
        let client_config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));
        let mut client = client;
        client
            .write_all(b"PROXY TCP4 198.51.100.10 127.0.0.1 12345 443\r\n")
            .await
            .unwrap();
        let mut client = connector
            .connect(ServerName::try_from("localhost").unwrap(), client)
            .await
            .unwrap();
        let wire = wire();
        let request = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/dns-message\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        client.write_all(&request).await.unwrap();
        let DohSessionEvent::Request(inbound) = server.await.unwrap() else {
            panic!("expected a valid DoH request");
        };
        assert_eq!(
            inbound.request().context.client.client_addr,
            Some("198.51.100.10".parse::<IpAddr>().unwrap())
        );
    }

    #[tokio::test]
    async fn failed_tls_handshake_does_not_poison_listener() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let certificate_der = certified.cert.der().to_vec();
        let material = Arc::new(TlsServerMaterial {
            certificate_chain: vec![certificate_der.clone()],
            private_key: certified.signing_key.serialize_der(),
        });
        let factory = SystemSocketFactory::new();
        let cancellation = Cancellation::new();
        let prepared = factory
            .prepare(
                SocketSpec {
                    kind: SocketKind::Tcp,
                    address: "127.0.0.1:0".parse().unwrap(),
                    reuse_port: false,
                    v6_only: false,
                },
                Deadline::new(std::time::Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        let activated = prepared.activate().unwrap();
        let ActivatedSocketHandle::Tcp(listener) = activated.socket_handle().unwrap() else {
            unreachable!();
        };
        let address = listener.local_addr().unwrap();
        let adapter = DohAdapter::new_with_tls(
            Arc::clone(&listener),
            crate::config::DohBindingRef {
                listener_id: "doh".to_owned(),
                endpoint_id: "tls".to_owned(),
            },
            vec![DohRoutePattern::new("/dns", "default").unwrap()],
            RuntimeRevision(1),
            doh_capabilities(),
            Duration::from_secs(3),
            DohClientIpPolicy::default(),
            Some(material),
        )
        .unwrap();

        // 非 TLS 客户端只应终止自己的 session，底层 listener 仍须继续接收连接。
        let mut invalid_client = TcpStream::connect(address).await.unwrap();
        let mut invalid_session = adapter
            .accept_session(&cancellation)
            .await
            .unwrap()
            .unwrap();
        invalid_client
            .write_all(b"GET / HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        invalid_client.shutdown().await.unwrap();
        let handshake_error = match invalid_session.receive(&cancellation).await {
            Err(error) => error,
            Ok(_) => panic!("非 TLS 输入不应形成 DoH session event"),
        };
        assert_eq!(handshake_error.operation(), "system_socket.tls_handshake");

        let client = TcpStream::connect(address).await.unwrap();
        let mut session = adapter
            .accept_session(&cancellation)
            .await
            .unwrap()
            .unwrap();
        let server = tokio::spawn(async move { session.receive(&Cancellation::new()).await });
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(certificate_der)).unwrap();
        let client_config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));
        let mut client = connector
            .connect(ServerName::try_from("localhost").unwrap(), client)
            .await
            .unwrap();
        let wire = wire();
        client
            .write_all(&request(
                "POST",
                "/dns",
                &format!(
                    "Content-Type: application/dns-message\r\nContent-Length: {}\r\n",
                    wire.len()
                ),
                &wire,
            ))
            .await
            .unwrap();

        assert!(matches!(
            server.await.unwrap().unwrap(),
            DohSessionEvent::Request(_)
        ));
    }

    #[test]
    fn parses_get_and_restores_wire_id_metadata() {
        let wire = wire();
        let encoded = base64url(&wire);
        let request_bytes = request("GET", &format!("/dns/{}/?dns={encoded}", "client"), "", &[]);

        let parsed = try_parse_request(&request_bytes).unwrap().unwrap();
        assert_eq!(parsed.method, DohHttpMethod::Get);
        assert_eq!(parsed.path, "/dns/client/");
        assert_eq!(parsed.query.id.value(), 0x1234);
        assert_eq!(parsed.query.query.as_message().metadata.id, 0);
        assert_eq!(parsed.consumed_bytes, request_bytes.len());
    }

    #[test]
    fn parses_post_and_requires_supported_media_type() {
        let wire = wire();
        let request_bytes = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/dns-message\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        assert_eq!(
            try_parse_request(&request_bytes).unwrap().unwrap().wire,
            wire
        );

        let mixed_case = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: Application/DNS-Message\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        assert_eq!(try_parse_request(&mixed_case).unwrap().unwrap().wire, wire);

        let invalid = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/octet-stream\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        assert_eq!(
            try_parse_request(&invalid),
            Err(DohHttpError::UnsupportedMediaType)
        );

        let parameterized = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/dns-message; charset=utf-8\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        assert_eq!(
            try_parse_request(&parameterized),
            Err(DohHttpError::UnsupportedMediaType)
        );
    }

    /// 验证歧义 body framing 和无界 header 在进入 DNS parser 前 fail-closed。
    #[test]
    fn rejects_ambiguous_or_unbounded_http_framing() {
        let wire = wire();
        let duplicate_length = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/dns-message\r\nContent-Length: {}\r\nContent-Length: {}\r\n",
                wire.len(),
                wire.len()
            ),
            &wire,
        );
        assert_eq!(
            try_parse_request(&duplicate_length),
            Err(DohHttpError::Malformed)
        );

        let transfer_encoding = request(
            "POST",
            "/dns",
            "Content-Type: application/dns-message\r\nTransfer-Encoding: chunked\r\n",
            &wire,
        );
        assert_eq!(
            try_parse_request(&transfer_encoding),
            Err(DohHttpError::UnsupportedTransferEncoding)
        );

        let too_many_headers = (0..=MAX_HEADER_COUNT)
            .map(|index| format!("X-Test-{index}: value\r\n"))
            .collect::<String>();
        let encoded = base64url(&wire);
        assert_eq!(
            try_parse_request(&request(
                "GET",
                &format!("/dns?dns={encoded}"),
                &too_many_headers,
                &[],
            )),
            Err(DohHttpError::Malformed)
        );

        let oversized_headers = format!("X-Large: {}\r\n", "a".repeat(MAX_DOH_HEADER_BYTES));
        assert_eq!(
            try_parse_request(&request(
                "GET",
                &format!("/dns?dns={encoded}"),
                &oversized_headers,
                &[],
            )),
            Err(DohHttpError::Malformed)
        );
    }

    #[test]
    fn rejects_missing_or_duplicate_http11_host() {
        let encoded = base64url(&wire());
        let missing = format!("GET /dns?dns={encoded} HTTP/1.1\r\n\r\n").into_bytes();
        assert_eq!(try_parse_request(&missing), Err(DohHttpError::InvalidHost));
        assert!(DohHttpError::InvalidHost.should_close());

        let duplicate = request(
            "GET",
            &format!("/dns?dns={encoded}"),
            "Host: duplicate.test\r\n",
            &[],
        );
        assert_eq!(
            try_parse_request(&duplicate),
            Err(DohHttpError::InvalidHost)
        );

        for invalid_host in ["", "bad host", "user@doh.test", "doh.test/path"] {
            let invalid =
                format!("GET /dns?dns={encoded} HTTP/1.1\r\nHost: {invalid_host}\r\n\r\n")
                    .into_bytes();
            assert_eq!(try_parse_request(&invalid), Err(DohHttpError::InvalidHost));
        }

        for valid_host in ["doh.test:443", "[::1]:443"] {
            let valid = format!("GET /dns?dns={encoded} HTTP/1.1\r\nHost: {valid_host}\r\n\r\n")
                .into_bytes();
            assert!(try_parse_request(&valid).unwrap().is_some());
        }

        let http10 = format!("GET /dns?dns={encoded} HTTP/1.0\r\n\r\n").into_bytes();
        let parsed = try_parse_request(&http10).unwrap().unwrap();
        assert!(parsed.connection_close);
    }

    /// 验证 Content-Length 只接受 RFC 定义的十进制数字形式。
    #[test]
    fn rejects_signed_or_non_decimal_content_length() {
        let wire = wire();
        for content_length in [
            format!("+{}", wire.len()),
            format!("-{}", wire.len()),
            "0x20".into(),
        ] {
            let invalid = request(
                "POST",
                "/dns",
                &format!(
                    "Content-Type: application/dns-message\r\nContent-Length: {content_length}\r\n"
                ),
                &wire,
            );
            assert_eq!(try_parse_request(&invalid), Err(DohHttpError::Malformed));
        }
    }

    #[test]
    fn rejects_invalid_request_line_tokens() {
        let encoded = base64url(&wire());
        let invalid_method =
            format!("G\tET /dns?dns={encoded} HTTP/1.1\r\nHost: doh.test\r\n\r\n").into_bytes();
        assert_eq!(
            try_parse_request(&invalid_method),
            Err(DohHttpError::Malformed)
        );

        let invalid_target =
            format!("GET /dns?dns={encoded}\0 HTTP/1.1\r\nHost: doh.test\r\n\r\n").into_bytes();
        assert_eq!(
            try_parse_request(&invalid_target),
            Err(DohHttpError::Malformed)
        );

        let unsupported =
            format!("PUT /dns?dns={encoded} HTTP/1.1\r\nHost: doh.test\r\n\r\n").into_bytes();
        assert_eq!(
            try_parse_request(&unsupported),
            Err(DohHttpError::MethodNotAllowed)
        );
    }

    #[test]
    fn parses_forwarded_headers_and_selects_first_untrusted_address_from_right() {
        let wire = wire();
        let request_bytes = request(
            "GET",
            &format!("/dns?dns={}", base64url(&wire)),
            "X-Forwarded-For: 198.51.100.10, 127.0.0.2\r\n",
            &[],
        );
        let parsed = try_parse_request(&request_bytes).unwrap().unwrap();
        assert_eq!(
            parsed.forwarded_headers.x_forwarded_for.as_deref(),
            Some("198.51.100.10, 127.0.0.2")
        );
        let policy = DohClientIpPolicy {
            source: ClientIpSource::ForwardedHeader,
            header: Some(ForwardedHeader::XForwardedFor),
            trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
            on_missing: ForwardedDisposition::Reject,
            on_invalid: ForwardedDisposition::Reject,
        };
        assert_eq!(
            policy
                .resolve(
                    SocketAddr::from(([127, 0, 0, 1], 8053)),
                    &parsed.forwarded_headers,
                    None,
                )
                .unwrap(),
            "198.51.100.10".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn forwarded_header_policy_rejects_untrusted_peer_and_can_fall_back_on_invalid() {
        let policy = DohClientIpPolicy {
            source: ClientIpSource::ForwardedHeader,
            header: Some(ForwardedHeader::XRealIp),
            trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
            on_missing: ForwardedDisposition::Reject,
            on_invalid: ForwardedDisposition::UsePeer,
        };
        let headers = ParsedForwardedHeaders {
            x_real_ip: Some("not-an-ip".to_owned()),
            ..ParsedForwardedHeaders::default()
        };
        assert_eq!(
            policy.resolve(SocketAddr::from(([192, 0, 2, 1], 8053)), &headers, None),
            Err(ClientIpResolutionError::UntrustedPeer)
        );
        assert_eq!(
            policy
                .resolve(SocketAddr::from(([127, 0, 0, 1], 8053)), &headers, None)
                .unwrap(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn maps_http_protocol_boundaries_to_stable_statuses() {
        assert_eq!(DohHttpError::MethodNotAllowed.status().code(), 405);
        assert_eq!(DohHttpError::UnsupportedMediaType.status().code(), 415);
        assert_eq!(DohHttpError::PayloadTooLarge.status().code(), 413);
        assert_eq!(DohHttpError::UriTooLong.status().code(), 414);
        let response = encode_http_error(DohHttpError::MethodNotAllowed);
        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
        assert!(text.contains("Allow: GET, POST\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
    }

    #[test]
    fn rejects_missing_duplicate_and_padded_get_parameters() {
        let wire = wire();
        let encoded = base64url(&wire);
        let missing = request("GET", "/dns", "", &[]);
        assert_eq!(
            try_parse_request(&missing),
            Err(DohHttpError::MissingDnsParameter)
        );
        let duplicate = request("GET", &format!("/dns?dns={encoded}&dns={encoded}"), "", &[]);
        assert_eq!(
            try_parse_request(&duplicate),
            Err(DohHttpError::DuplicateDnsParameter)
        );
        let padded = request("GET", &format!("/dns?dns={encoded}="), "", &[]);
        assert_eq!(
            try_parse_request(&padded),
            Err(DohHttpError::InvalidDnsParameter)
        );
        let percent_encoded = request(
            "GET",
            &format!("/dns?dns={}", encoded.replace('-', "%2D")),
            "",
            &[],
        );
        assert_eq!(
            try_parse_request(&percent_encoded)
                .unwrap()
                .unwrap()
                .query
                .id
                .value(),
            0x1234
        );
    }

    #[test]
    fn returns_incomplete_until_headers_and_body_are_available() {
        let wire = wire();
        let full = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/dns-message\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        let header_end = find_subslice(&full, b"\r\n\r\n").unwrap() + 4;
        assert_eq!(try_parse_request(&full[..header_end - 1]).unwrap(), None);
        assert_eq!(
            try_parse_request(&full[..header_end + wire.len() - 1]).unwrap(),
            None
        );
    }

    #[test]
    fn request_target_and_header_fields_use_independent_limits() {
        let encoded = base64url(&wire());
        let suffix = format!("?dns={encoded}");
        let path_bytes = MAX_DOH_REQUEST_TARGET_BYTES - suffix.len();
        let target = format!("/{}{}", "a".repeat(path_bytes - 1), suffix);
        assert_eq!(target.len(), MAX_DOH_REQUEST_TARGET_BYTES);

        let parsed = try_parse_request(&request("GET", &target, "", &[]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.path.len(), path_bytes);

        let oversized = format!("{target}x");
        assert_eq!(
            try_parse_request(&request("GET", &oversized, "", &[])),
            Err(DohHttpError::UriTooLong)
        );
        const { assert!(MAX_DOH_BUFFER_BYTES > MAX_DOH_REQUEST_HEAD_BYTES) };
    }

    #[test]
    fn route_pattern_matches_exact_and_client_id_segments() {
        let exact = DohRoutePattern::new("/dns", "default").unwrap();
        assert!(exact.matches("/dns").is_some());
        assert!(exact.matches("/dns/extra").is_none());

        let templated = DohRoutePattern::new("/dns/{client_id}", "inner").unwrap();
        let matched = templated.matches("/dns/abc-123").unwrap();
        assert_eq!(matched.strategy, "inner");
        assert_eq!(matched.client_id.unwrap().as_str(), "abc-123");
        assert!(templated.matches("/dns/a/b").is_none());
    }

    #[test]
    fn route_pattern_rejects_embedded_or_repeated_placeholder() {
        assert_eq!(
            DohRoutePattern::new("/dns/{client_id}/x/{client_id}", "default"),
            Err(DohRouteError::InvalidPlaceholder)
        );
        assert_eq!(
            DohRoutePattern::new("/dns/pre{client_id}", "default"),
            Err(DohRouteError::InvalidPlaceholder)
        );
    }

    #[derive(Default)]
    struct FakeDohListener {
        connections: Mutex<VecDeque<Box<dyn TcpConnectionHandle>>>,
    }

    impl TcpListenerHandle for FakeDohListener {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(SocketAddr::from(([127, 0, 0, 1], 8355)))
        }

        fn accept<'a>(
            &'a self,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Option<Box<dyn TcpConnectionHandle>>, PortError>> {
            let connection = self.connections.lock().unwrap().pop_front();
            Box::pin(async move { Ok(connection) })
        }
    }

    struct FakeDohConnection {
        peer: SocketAddr,
        chunks: Mutex<VecDeque<Result<TcpReadChunkResult, PortError>>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl TcpConnectionHandle for FakeDohConnection {
        fn peer_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(self.peer)
        }

        fn read_exact<'a>(
            &'a mut self,
            _length: usize,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<TcpReadResult, PortError>> {
            Box::pin(async {
                Err(PortError::new(
                    PortErrorClass::Internal,
                    "test.doh.read_exact",
                ))
            })
        }

        fn read_chunk<'a>(
            &'a mut self,
            _max_bytes: usize,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<TcpReadChunkResult, PortError>> {
            let result = self
                .chunks
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(TcpReadChunkResult::CleanEof));
            Box::pin(async move { result })
        }

        fn write_all<'a>(
            &'a mut self,
            payload: Vec<u8>,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<(), PortError>> {
            self.writes.lock().unwrap().push(payload);
            Box::pin(async { Ok(()) })
        }

        fn shutdown(&mut self) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn doh_capabilities() -> TransportCapabilities {
        TransportCapabilities {
            class: TransportClass::Multiplexed,
            cache_compatibility: CacheCompatibilityKey(1),
        }
    }

    #[tokio::test]
    async fn session_maps_http_request_to_core_and_writes_dns_response() {
        let wire = wire();
        let request = request(
            "POST",
            "/dns/client",
            &format!(
                "Content-Type: application/dns-message\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        let writes = Arc::new(Mutex::new(Vec::new()));
        let connection = FakeDohConnection {
            peer: SocketAddr::from(([127, 0, 0, 1], 40000)),
            chunks: Mutex::new(VecDeque::from([Ok(TcpReadChunkResult::Data(request))])),
            writes: Arc::clone(&writes),
        };
        let listener = Arc::new(FakeDohListener::default());
        listener
            .connections
            .lock()
            .unwrap()
            .push_back(Box::new(connection));
        let adapter = DohAdapter::new(
            listener,
            DohBindingRef {
                listener_id: "doh".to_owned(),
                endpoint_id: "plain".to_owned(),
            },
            vec![DohRoutePattern::new("/dns/{client_id}", "default").unwrap()],
            RuntimeRevision(1),
            doh_capabilities(),
            Duration::from_secs(1),
        )
        .unwrap();
        let cancellation = Cancellation::new();
        let mut session = adapter
            .accept_session(&cancellation)
            .await
            .unwrap()
            .unwrap();

        let event = session.receive(&cancellation).await.unwrap();
        let DohSessionEvent::Request(inbound) = event else {
            panic!("expected a valid DoH request");
        };
        assert_eq!(
            inbound.request().context.meta.connection_id,
            Some(ConnectionId(1))
        );
        assert_eq!(
            inbound
                .request()
                .context
                .client
                .client_id
                .as_ref()
                .unwrap()
                .as_str(),
            "client"
        );
        assert_eq!(
            dispatch_inbound(&ServFailCore, inbound).await.unwrap(),
            crate::dns::DispatchOutcome::Responded
        );

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        let response = String::from_utf8_lossy(&writes[0]);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: application/dns-message\r\n"));
        assert!(response.contains("Cache-Control: no-store\r\n"));
        let body_start = writes[0]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HTTP response headers")
            + 4;
        let dns_response = Message::from_vec(&writes[0][body_start..]).unwrap();
        assert_eq!(dns_response.metadata.id, 0x1234);
    }

    #[tokio::test]
    async fn session_restores_forwarded_client_address_for_trusted_peer() {
        let wire = wire();
        let request = request(
            "GET",
            &format!("/dns?dns={}", base64url(&wire)),
            "X-Forwarded-For: 198.51.100.10, 127.0.0.2\r\n",
            &[],
        );
        let listener = Arc::new(FakeDohListener::default());
        listener
            .connections
            .lock()
            .unwrap()
            .push_back(Box::new(FakeDohConnection {
                peer: SocketAddr::from(([127, 0, 0, 1], 40000)),
                chunks: Mutex::new(VecDeque::from([Ok(TcpReadChunkResult::Data(request))])),
                writes: Arc::new(Mutex::new(Vec::new())),
            }));
        let adapter = DohAdapter::new_with_policy(
            listener,
            DohBindingRef {
                listener_id: "doh".to_owned(),
                endpoint_id: "forwarded".to_owned(),
            },
            vec![DohRoutePattern::new("/dns", "default").unwrap()],
            RuntimeRevision(1),
            doh_capabilities(),
            Duration::from_secs(1),
            DohClientIpPolicy {
                source: ClientIpSource::ForwardedHeader,
                header: Some(ForwardedHeader::XForwardedFor),
                trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
                on_missing: ForwardedDisposition::Reject,
                on_invalid: ForwardedDisposition::Reject,
            },
        )
        .unwrap();
        let cancellation = Cancellation::new();
        let mut session = adapter
            .accept_session(&cancellation)
            .await
            .unwrap()
            .unwrap();
        let DohSessionEvent::Request(inbound) = session.receive(&cancellation).await.unwrap()
        else {
            panic!("expected a valid DoH request");
        };
        assert_eq!(
            inbound.request().context.client.client_addr,
            Some("198.51.100.10".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn parses_proxy_v1_and_v2_source_addresses_with_bounded_headers() {
        let v1 = b"PROXY TCP4 198.51.100.10 127.0.0.1 12345 8053\r\n";
        assert_eq!(
            parse_proxy_header(&v1[..5]),
            Ok(ProxyHeaderParse::Incomplete)
        );
        assert_eq!(
            parse_proxy_header(v1),
            Ok(ProxyHeaderParse::Complete {
                consumed: v1.len(),
                client: "198.51.100.10".parse().unwrap(),
            })
        );

        let source = std::net::Ipv6Addr::LOCALHOST;
        let destination = "2001:db8::2".parse::<std::net::Ipv6Addr>().unwrap();
        let mut v2 = PROXY_V2_SIGNATURE.to_vec();
        v2.extend_from_slice(&[0x21, 0x21, 0, 36]);
        v2.extend_from_slice(&source.octets());
        v2.extend_from_slice(&destination.octets());
        v2.extend_from_slice(&12345_u16.to_be_bytes());
        v2.extend_from_slice(&8053_u16.to_be_bytes());
        assert_eq!(
            parse_proxy_header(&v2[..16]),
            Ok(ProxyHeaderParse::Incomplete)
        );
        assert_eq!(
            parse_proxy_header(&v2),
            Ok(ProxyHeaderParse::Complete {
                consumed: v2.len(),
                client: IpAddr::V6(source),
            })
        );
    }

    #[tokio::test]
    async fn session_restores_proxy_protocol_client_address_for_trusted_peer() {
        let wire = wire();
        let mut request_bytes = b"PROXY TCP4 198.51.100.10 127.0.0.1 12345 8053\r\n".to_vec();
        request_bytes.extend_from_slice(&request(
            "GET",
            &format!("/dns?dns={}", base64url(&wire)),
            "",
            &[],
        ));
        let listener = Arc::new(FakeDohListener::default());
        listener
            .connections
            .lock()
            .unwrap()
            .push_back(Box::new(FakeDohConnection {
                peer: SocketAddr::from(([127, 0, 0, 1], 40000)),
                chunks: Mutex::new(VecDeque::from([Ok(TcpReadChunkResult::Data(
                    request_bytes,
                ))])),
                writes: Arc::new(Mutex::new(Vec::new())),
            }));
        let adapter = DohAdapter::new_with_policy(
            listener,
            DohBindingRef {
                listener_id: "doh".to_owned(),
                endpoint_id: "proxy".to_owned(),
            },
            vec![DohRoutePattern::new("/dns", "default").unwrap()],
            RuntimeRevision(1),
            doh_capabilities(),
            Duration::from_secs(1),
            DohClientIpPolicy {
                source: ClientIpSource::ProxyProtocol,
                header: None,
                trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
                on_missing: ForwardedDisposition::Reject,
                on_invalid: ForwardedDisposition::Reject,
            },
        )
        .unwrap();
        let cancellation = Cancellation::new();
        let mut session = adapter
            .accept_session(&cancellation)
            .await
            .unwrap()
            .unwrap();
        let DohSessionEvent::Request(inbound) = session.receive(&cancellation).await.unwrap()
        else {
            panic!("expected a valid DoH request");
        };
        assert_eq!(
            inbound.request().context.client.client_addr,
            Some("198.51.100.10".parse::<IpAddr>().unwrap())
        );
    }

    #[tokio::test]
    async fn session_reports_route_error_without_confusing_it_with_dns_failure() {
        let wire = wire();
        let encoded = base64url(&wire);
        let request = request("GET", &format!("/unknown?dns={encoded}"), "", &[]);
        let listener = Arc::new(FakeDohListener::default());
        listener
            .connections
            .lock()
            .unwrap()
            .push_back(Box::new(FakeDohConnection {
                peer: SocketAddr::from(([127, 0, 0, 1], 40000)),
                chunks: Mutex::new(VecDeque::from([Ok(TcpReadChunkResult::Data(request))])),
                writes: Arc::new(Mutex::new(Vec::new())),
            }));
        let adapter = DohAdapter::new(
            listener,
            DohBindingRef {
                listener_id: "doh".to_owned(),
                endpoint_id: "plain".to_owned(),
            },
            vec![DohRoutePattern::new("/dns", "default").unwrap()],
            RuntimeRevision(1),
            doh_capabilities(),
            Duration::from_secs(1),
        )
        .unwrap();
        let cancellation = Cancellation::new();
        let mut session = adapter
            .accept_session(&cancellation)
            .await
            .unwrap()
            .unwrap();
        let event = session.receive(&cancellation).await.unwrap();
        assert!(matches!(
            event,
            DohSessionEvent::HttpError {
                error: DohHttpError::NotFound,
                close: false
            }
        ));
    }
}
