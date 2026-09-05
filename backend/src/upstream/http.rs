//! 基于 Tokio TCP 的 plain HTTP/1.1 DoH transport adapter。
//!
//! 该 adapter 只负责 `http://` endpoint 的一次请求/响应交换；HTTPS/TLS、proxy
//! 和连接池由后续 adapter 提供。所有读取均受 header/body/deadline 上限约束。

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, lookup_host};
use tokio::sync::Semaphore;
use url::Url;

use crate::config::resolve::{ConfigId, ResolvedUpstream};
use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::{
    Clock, OutboundAddressResolver, OutboundDialer, OutboundStream, TcpReadChunkResult,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};
use crate::runtime::SystemClock;

use super::{
    AddressCachePolicy, AddressResolutionRequest, AddressResolutionState,
    BootstrapConnectorRegistry, BootstrapResolution, BootstrapResolver, BootstrapResolverError,
    DEFAULT_BOOTSTRAP_MAX_TTL, DOH_MEDIA_TYPE, DohHttpRequest, DohHttpResponseOwned,
    DohHttpTransport, MAX_DOH_RESPONSE_BODY_BYTES, NameResolution, OutboundProfile,
    Socks5ConnectError, Socks5Connector, Socks5HandshakeError, SystemResolverResolution,
};

const MAX_HTTP_HEADER_BYTES: usize = 32 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;

#[cfg(test)]
#[path = "address_cache_tests.rs"]
mod address_cache_tests;

/// DoH HTTP adapter 交给地址解析 port 的安全请求元数据。
#[derive(Clone, Eq, PartialEq)]
pub struct DohAddressRequest {
    host: Arc<str>,
    port: u16,
    bootstrap: Option<ConfigId>,
}

impl std::fmt::Debug for DohAddressRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DohAddressRequest")
            .field("has_host", &true)
            .field("port", &self.port)
            .field("has_bootstrap", &self.bootstrap.is_some())
            .finish()
    }
}

impl DohAddressRequest {
    pub(crate) fn new(host: &str, port: u16, bootstrap: Option<ConfigId>) -> Self {
        Self {
            host: Arc::from(host),
            port,
            bootstrap,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    pub fn bootstrap(&self) -> Option<&ConfigId> {
        self.bootstrap.as_ref()
    }
}

/// DoH HTTP adapter 使用的地址解析 port。
///
/// `connect_ip` 由调用方显式提供时不会调用该 port；配置绑定的 resolver
/// 在此边界缓存 bootstrap 地址，不改变 HTTP/DNS 协议身份。
pub trait DohAddressResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        request: DohAddressRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Vec<SocketAddr>, PortError>>;
}

#[derive(Clone)]
pub struct TokioDohAddressResolver {
    bootstrap: Option<Arc<BootstrapConnectorRegistry>>,
    binding: Option<Arc<BoundAddressCache>>,
    clock: Arc<dyn Clock>,
}

struct BoundAddressCache {
    expected: DohAddressRequest,
    request: AddressResolutionRequest,
    state: Mutex<AddressResolutionState>,
    fill: Semaphore,
}

impl BoundAddressCache {
    fn cached(&self, now: Instant, failed: bool) -> Option<Vec<SocketAddr>> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .resolve(
                &self.request,
                if failed {
                    BootstrapResolution::Failed
                } else {
                    BootstrapResolution::NotAttempted
                },
                SystemResolverResolution::Failed,
                now,
            )
            .ok()
            .map(|answer| {
                answer
                    .addresses()
                    .iter()
                    .map(|ip| SocketAddr::new(*ip, self.expected.port()))
                    .collect()
            })
    }
}

impl std::fmt::Debug for TokioDohAddressResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokioDohAddressResolver")
            .field("has_bootstrap_registry", &self.bootstrap.is_some())
            .field("has_config_binding", &self.binding.is_some())
            .finish()
    }
}

impl Default for TokioDohAddressResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioDohAddressResolver {
    pub fn new() -> Self {
        Self {
            bootstrap: None,
            binding: None,
            clock: Arc::new(SystemClock::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_bootstrap_registry(bootstrap: Arc<BootstrapConnectorRegistry>) -> Self {
        Self {
            bootstrap: Some(bootstrap),
            ..Self::new()
        }
    }

    /// 每个 connector 绑定一份配置身份与缓存，配置换代不能复用旧地址状态。
    pub(crate) fn for_upstream(
        upstream: &ResolvedUpstream,
        bootstrap: Arc<BootstrapConnectorRegistry>,
    ) -> Result<Self, PortError> {
        let invalid = || {
            PortError::new(
                PortErrorClass::InvalidInput,
                "doh_http_transport.bind_resolver",
            )
        };
        let ResolvedUpstream::Doh {
            address,
            bootstrap: bootstrap_id,
            ..
        } = upstream
        else {
            return Err(invalid());
        };
        let host = address.host_str().ok_or_else(invalid)?;
        let port = address.port_or_known_default().ok_or_else(invalid)?;
        let request = AddressResolutionRequest::from_resolved(upstream).map_err(|_| invalid())?;
        Ok(Self {
            bootstrap: Some(bootstrap),
            binding: Some(Arc::new(BoundAddressCache {
                expected: DohAddressRequest::new(host, port, bootstrap_id.clone()),
                request,
                state: Mutex::new(AddressResolutionState::new(
                    AddressCachePolicy::new(Duration::ZERO, DEFAULT_BOOTSTRAP_MAX_TTL)
                        .expect("production bootstrap TTL bounds are valid"),
                )),
                fill: Semaphore::new(1),
            })),
            clock: Arc::new(SystemClock::new()),
        })
    }

    fn check_budget(
        &self,
        deadline: Deadline,
        cancellation: &Cancellation,
    ) -> Result<(), PortError> {
        if let Some(reason) = cancellation.reason() {
            return Err(PortError::new(
                PortErrorClass::Cancelled(reason),
                "doh_http_transport.resolve",
            ));
        }
        if deadline.is_expired(self.clock.monotonic_now()) {
            return Err(PortError::new(
                PortErrorClass::Timeout,
                "doh_http_transport.resolve",
            ));
        }
        Ok(())
    }
}

impl DohAddressResolver for TokioDohAddressResolver {
    fn resolve<'a>(
        &'a self,
        request: DohAddressRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Vec<SocketAddr>, PortError>> {
        Box::pin(async move {
            self.check_budget(deadline, cancellation)?;
            if self
                .binding
                .as_ref()
                .is_some_and(|binding| binding.expected != request)
            {
                return Err(PortError::new(
                    PortErrorClass::InvalidInput,
                    "doh_http_transport.resolve",
                )
                .with_safe_context("address request does not match connector configuration"));
            }
            if let Some(bootstrap_id) = request.bootstrap() {
                if let Some(addresses) = self
                    .binding
                    .as_ref()
                    .and_then(|binding| binding.cached(self.clock.monotonic_now(), false))
                {
                    self.check_budget(deadline, cancellation)?;
                    return Ok(addresses);
                }
                // 许可只串行化缓存查填；取消/drop 自动释放，状态锁绝不跨网络等待。
                let _permit = if let Some(binding) = &self.binding {
                    let permit = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return Err(PortError::new(PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled)), "doh_http_transport.resolve")),
                        _ = self.clock.sleep_until(deadline) => return Err(PortError::new(PortErrorClass::Timeout, "doh_http_transport.resolve")),
                        permit = binding.fill.acquire() => permit.map_err(|_| PortError::new(PortErrorClass::Unavailable, "doh_http_transport.resolve"))?,
                    };
                    self.check_budget(deadline, cancellation)?;
                    if let Some(addresses) = binding.cached(self.clock.monotonic_now(), false) {
                        return Ok(addresses);
                    }
                    Some(permit)
                } else {
                    None
                };
                let Some(registry) = self.bootstrap.as_ref() else {
                    return Err(PortError::new(
                        PortErrorClass::Unavailable,
                        "doh_http_transport.resolve",
                    )
                    .with_safe_context("bootstrap resolver is not configured"));
                };
                let Some(connector) = registry.get(bootstrap_id) else {
                    return Err(PortError::new(
                        PortErrorClass::Unavailable,
                        "doh_http_transport.resolve",
                    )
                    .with_safe_context("bootstrap connector is not registered"));
                };
                let resolver = BootstrapResolver::new(connector);
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(PortError::new(PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled)), "doh_http_transport.resolve")),
                    _ = self.clock.sleep_until(deadline) => return Err(PortError::new(PortErrorClass::Timeout, "doh_http_transport.resolve")),
                    answer = resolver.resolve_with_budget(request.host(), deadline, cancellation, self.clock.as_ref()) => answer.map_err(map_bootstrap_error),
                };
                self.check_budget(deadline, cancellation)?;
                let answer = match result {
                    Ok(answer) => answer,
                    Err(error) => {
                        if !matches!(
                            error.class(),
                            PortErrorClass::Timeout | PortErrorClass::Cancelled(_)
                        ) && let Some(addresses) = self
                            .binding
                            .as_ref()
                            .and_then(|binding| binding.cached(self.clock.monotonic_now(), true))
                        {
                            return Ok(addresses);
                        }
                        return Err(error);
                    }
                };
                if let Some(binding) = &self.binding {
                    let answer = binding
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .resolve(
                            &binding.request,
                            BootstrapResolution::Answer(answer),
                            SystemResolverResolution::Failed,
                            self.clock.monotonic_now(),
                        )
                        .map_err(|_| {
                            PortError::new(
                                PortErrorClass::Unavailable,
                                "doh_http_transport.resolve",
                            )
                        })?;
                    return Ok(answer
                        .addresses()
                        .iter()
                        .map(|ip| SocketAddr::new(*ip, request.port()))
                        .collect());
                }
                return Ok(answer
                    .addresses()
                    .iter()
                    .copied()
                    .map(|address| SocketAddr::new(address, request.port()))
                    .collect());
            }
            let mut addresses = await_io(
                lookup_host((request.host(), request.port())),
                deadline,
                cancellation,
                "doh_http_transport.resolve",
            )
            .await?;
            let addresses: Vec<_> = addresses.by_ref().collect();
            if addresses.is_empty() {
                return Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "doh_http_transport.resolve",
                ));
            }
            Ok(addresses)
        })
    }
}

fn map_bootstrap_error(error: BootstrapResolverError) -> PortError {
    match error {
        BootstrapResolverError::InvalidName | BootstrapResolverError::QueryBuild => {
            PortError::new(PortErrorClass::InvalidInput, "doh_http_transport.resolve")
                .with_safe_context("bootstrap resolver query was invalid")
        }
        BootstrapResolverError::Cancelled { reason, .. } => PortError::new(
            PortErrorClass::Cancelled(reason),
            "doh_http_transport.resolve",
        ),
        BootstrapResolverError::Transport { class, .. } => {
            let port_class = match class {
                crate::ports::exchange::TransportFailureClass::Timeout => PortErrorClass::Timeout,
                crate::ports::exchange::TransportFailureClass::ResourceExhausted => {
                    PortErrorClass::ResourceExhausted
                }
                crate::ports::exchange::TransportFailureClass::ProtocolViolation => {
                    PortErrorClass::ProtocolViolation
                }
                _ => PortErrorClass::Unavailable,
            };
            PortError::new(port_class, "doh_http_transport.resolve")
                .with_safe_context("bootstrap resolver exchange failed")
        }
        BootstrapResolverError::NoAddress { .. } => {
            PortError::new(PortErrorClass::Unavailable, "doh_http_transport.resolve")
                .with_safe_context("bootstrap resolver returned no address")
        }
    }
}

#[derive(Clone)]
pub struct TokioDohHttpTransport {
    resolver: Arc<dyn DohAddressResolver>,
}

impl std::fmt::Debug for TokioDohHttpTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokioDohHttpTransport")
            .field("has_resolver", &true)
            .finish()
    }
}

impl TokioDohHttpTransport {
    pub fn new() -> Self {
        Self::with_resolver(Arc::new(TokioDohAddressResolver::new()))
    }

    pub fn with_resolver<T>(resolver: Arc<T>) -> Self
    where
        T: DohAddressResolver + 'static,
    {
        Self { resolver }
    }
}

impl Default for TokioDohHttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl DohHttpTransport for TokioDohHttpTransport {
    fn post<'a>(
        &'a self,
        request: DohHttpRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
        Box::pin(async move { post_http(request, &*self.resolver, deadline, cancellation).await })
    }
}

#[derive(Clone)]
pub struct TokioSocks5DohHttpTransport<D> {
    proxy: OutboundProfile,
    connector: Arc<Socks5Connector<D>>,
    proxy_resolver: Arc<dyn OutboundAddressResolver>,
    target_resolver: Arc<dyn DohAddressResolver>,
}

impl<D> std::fmt::Debug for TokioSocks5DohHttpTransport<D> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokioSocks5DohHttpTransport")
            .field("proxy", &self.proxy)
            .field("has_proxy_resolver", &true)
            .field("has_target_resolver", &true)
            .finish()
    }
}

impl<D: OutboundDialer> TokioSocks5DohHttpTransport<D> {
    pub fn new(
        proxy: OutboundProfile,
        dialer: Arc<D>,
        proxy_resolver: Arc<dyn OutboundAddressResolver>,
        target_resolver: Arc<dyn DohAddressResolver>,
    ) -> Self {
        Self {
            proxy,
            connector: Arc::new(Socks5Connector::new(dialer)),
            proxy_resolver,
            target_resolver,
        }
    }
}

impl<D: OutboundDialer + 'static> DohHttpTransport for TokioSocks5DohHttpTransport<D> {
    fn post<'a>(
        &'a self,
        request: DohHttpRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
        Box::pin(async move {
            post_http_via_socks(
                request,
                &self.proxy,
                &self.connector,
                &*self.proxy_resolver,
                &*self.target_resolver,
                deadline,
                cancellation,
            )
            .await
        })
    }
}

async fn post_http_via_socks<D>(
    request: DohHttpRequest,
    proxy: &OutboundProfile,
    connector: &Socks5Connector<D>,
    proxy_resolver: &dyn OutboundAddressResolver,
    target_resolver: &dyn DohAddressResolver,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<DohHttpResponseOwned, PortError>
where
    D: OutboundDialer,
{
    let endpoint = request.endpoint();
    if endpoint.scheme() != "http" {
        return Err(
            PortError::new(PortErrorClass::Unavailable, "doh_http_transport.post")
                .with_safe_context("https requires a TLS adapter"),
        );
    }
    let host = request.host();
    if host.is_empty() || host.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err(PortError::new(
            PortErrorClass::InvalidInput,
            "doh_http_transport.post",
        ));
    }
    check_budget(deadline, cancellation, "doh_http_transport.post")?;
    let port = endpoint.port().unwrap_or(80);
    let target = proxy
        .target(
            host,
            port,
            request.connect_ip(),
            request.bootstrap().cloned(),
        )
        .map_err(|_| {
            PortError::new(
                PortErrorClass::InvalidInput,
                "doh_http_transport.proxy_target",
            )
        })?;
    let resolved_ip = if matches!(target.name_resolution(), NameResolution::Local) {
        let address = target_resolver
            .resolve(
                DohAddressRequest::new(host, port, request.bootstrap().cloned()),
                deadline,
                cancellation,
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                PortError::new(
                    PortErrorClass::Unavailable,
                    "doh_http_transport.target_resolve",
                )
            })?;
        Some(address.ip())
    } else {
        None
    };
    let mut stream = connector
        .connect_profile_with_resolver(
            proxy,
            proxy_resolver,
            &target,
            resolved_ip,
            deadline,
            cancellation,
        )
        .await
        .map_err(map_socks5_connect_error)?;

    let path = request_path(endpoint);
    let host_header = host_header(endpoint, host);
    let body = request.body();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_header}\r\nContent-Type: {DOH_MEDIA_TYPE}\r\nAccept: {DOH_MEDIA_TYPE}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.into_bytes(), deadline, cancellation)
        .await?;
    stream
        .write_all(body.to_vec(), deadline, cancellation)
        .await?;
    stream.shutdown().await?;
    read_response_stream(&mut *stream, deadline, cancellation).await
}

fn map_socks5_connect_error(error: Socks5ConnectError) -> PortError {
    match error {
        Socks5ConnectError::Target(_) => PortError::new(
            PortErrorClass::InvalidInput,
            "doh_http_transport.proxy_target",
        ),
        Socks5ConnectError::ProxyResolve(error) | Socks5ConnectError::Dial(error) => {
            remap_port_error(error, "doh_http_transport.proxy")
        }
        Socks5ConnectError::Handshake(error) => match error {
            Socks5HandshakeError::Transport(error) => {
                remap_port_error(error, "doh_http_transport.proxy")
            }
            Socks5HandshakeError::Protocol(_) => PortError::new(
                PortErrorClass::ProtocolViolation,
                "doh_http_transport.proxy",
            ),
            Socks5HandshakeError::CredentialsRequired => {
                PortError::new(PortErrorClass::InvalidInput, "doh_http_transport.proxy")
            }
            Socks5HandshakeError::ProxyRejected(_) => {
                PortError::new(PortErrorClass::Unavailable, "doh_http_transport.proxy")
            }
        },
    }
}

fn remap_port_error(error: PortError, operation: &'static str) -> PortError {
    let class = match error.class() {
        PortErrorClass::InvalidInput => PortErrorClass::InvalidInput,
        PortErrorClass::Timeout => PortErrorClass::Timeout,
        PortErrorClass::Cancelled(reason) => PortErrorClass::Cancelled(*reason),
        PortErrorClass::Unavailable => PortErrorClass::Unavailable,
        PortErrorClass::PermissionDenied => PortErrorClass::PermissionDenied,
        PortErrorClass::ResourceExhausted => PortErrorClass::ResourceExhausted,
        PortErrorClass::ProtocolViolation => PortErrorClass::ProtocolViolation,
        PortErrorClass::CorruptData => PortErrorClass::CorruptData,
        PortErrorClass::Internal => PortErrorClass::Internal,
    };
    PortError::new(class, operation)
}

async fn read_response_stream(
    stream: &mut dyn OutboundStream,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<DohHttpResponseOwned, PortError> {
    let mut bytes = Vec::with_capacity(1024);
    let (header_end, content_length) = loop {
        if bytes.len() > MAX_HTTP_HEADER_BYTES {
            return Err(PortError::new(
                PortErrorClass::ResourceExhausted,
                "doh_http_transport.read",
            )
            .with_safe_context("HTTP headers exceeded the limit"));
        }
        let chunk = stream
            .read_chunk(READ_CHUNK_BYTES, deadline, cancellation)
            .await?;
        let data = match chunk {
            TcpReadChunkResult::Data(data) => data,
            TcpReadChunkResult::CleanEof => {
                return Err(PortError::new(
                    PortErrorClass::ProtocolViolation,
                    "doh_http_transport.read",
                )
                .with_safe_context("HTTP response ended before headers"));
            }
        };
        bytes.extend_from_slice(&data);
        let Some(header_end) = find_header_end(&bytes) else {
            continue;
        };
        let content_length = parse_headers(&bytes[..header_end])?;
        break (header_end + 4, content_length);
    };
    if content_length > MAX_DOH_RESPONSE_BODY_BYTES {
        return Err(PortError::new(
            PortErrorClass::ResourceExhausted,
            "doh_http_transport.read",
        ));
    }
    let total = header_end.checked_add(content_length).ok_or_else(|| {
        PortError::new(PortErrorClass::ResourceExhausted, "doh_http_transport.read")
    })?;
    if bytes.len() > total {
        return Err(
            PortError::new(PortErrorClass::ProtocolViolation, "doh_http_transport.read")
                .with_safe_context("HTTP response contained trailing bytes"),
        );
    }
    while bytes.len() < total {
        let chunk = stream
            .read_chunk(READ_CHUNK_BYTES, deadline, cancellation)
            .await?;
        let data = match chunk {
            TcpReadChunkResult::Data(data) => data,
            TcpReadChunkResult::CleanEof => {
                return Err(PortError::new(
                    PortErrorClass::ProtocolViolation,
                    "doh_http_transport.read",
                )
                .with_safe_context("HTTP response ended before Content-Length"));
            }
        };
        if bytes.len().saturating_add(data.len()) > total {
            return Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "doh_http_transport.read",
            )
            .with_safe_context("HTTP response contained trailing bytes"));
        }
        bytes.extend_from_slice(&data);
    }
    let (status, content_type) = parse_status_and_content_type(&bytes[..header_end - 4])?;
    Ok(DohHttpResponseOwned {
        status,
        content_type,
        body: bytes[header_end..total].to_vec(),
    })
}

async fn post_http(
    request: DohHttpRequest,
    resolver: &dyn DohAddressResolver,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<DohHttpResponseOwned, PortError> {
    let endpoint = request.endpoint();
    if endpoint.scheme() != "http" {
        return Err(
            PortError::new(PortErrorClass::Unavailable, "doh_http_transport.post")
                .with_safe_context("https requires a TLS adapter"),
        );
    }
    let host = request.host();
    if host.is_empty() || host.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err(PortError::new(
            PortErrorClass::InvalidInput,
            "doh_http_transport.post",
        ));
    }
    check_budget(deadline, cancellation, "doh_http_transport.post")?;
    let port = endpoint.port().unwrap_or(80);
    let target = resolve_target(
        host,
        port,
        request.connect_ip(),
        request.bootstrap(),
        resolver,
        deadline,
        cancellation,
    )
    .await?;
    let mut stream = await_io(
        TcpStream::connect(target),
        deadline,
        cancellation,
        "doh_http_transport.connect",
    )
    .await?;

    let path = request_path(endpoint);
    let host_header = host_header(endpoint, host);
    let body = request.body();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_header}\r\nContent-Type: {DOH_MEDIA_TYPE}\r\nAccept: {DOH_MEDIA_TYPE}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    await_io(
        stream.write_all(head.as_bytes()),
        deadline,
        cancellation,
        "doh_http_transport.write",
    )
    .await?;
    await_io(
        stream.write_all(body),
        deadline,
        cancellation,
        "doh_http_transport.write",
    )
    .await?;
    await_io(
        stream.shutdown(),
        deadline,
        cancellation,
        "doh_http_transport.shutdown_write",
    )
    .await?;

    read_response(&mut stream, deadline, cancellation).await
}

async fn resolve_target(
    host: &str,
    port: u16,
    connect_ip: Option<IpAddr>,
    bootstrap: Option<&ConfigId>,
    resolver: &dyn DohAddressResolver,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<SocketAddr, PortError> {
    if let Some(address) = connect_ip {
        return Ok(SocketAddr::new(address, port));
    }
    resolver
        .resolve(
            DohAddressRequest::new(host, port, bootstrap.cloned()),
            deadline,
            cancellation,
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| PortError::new(PortErrorClass::Unavailable, "doh_http_transport.resolve"))
}

async fn read_response(
    stream: &mut TcpStream,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<DohHttpResponseOwned, PortError> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let (header_end, content_length) = loop {
        if bytes.len() > MAX_HTTP_HEADER_BYTES {
            return Err(PortError::new(
                PortErrorClass::ResourceExhausted,
                "doh_http_transport.read",
            )
            .with_safe_context("HTTP headers exceeded the limit"));
        }
        let count = await_io(
            stream.read(&mut chunk),
            deadline,
            cancellation,
            "doh_http_transport.read",
        )
        .await?;
        if count == 0 {
            return Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "doh_http_transport.read",
            )
            .with_safe_context("HTTP response ended before headers"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        let Some(header_end) = find_header_end(&bytes) else {
            continue;
        };
        let content_length = parse_headers(&bytes[..header_end])?;
        break (header_end + 4, content_length);
    };

    if content_length > super::MAX_DOH_RESPONSE_BODY_BYTES {
        return Err(
            PortError::new(PortErrorClass::ResourceExhausted, "doh_http_transport.read")
                .with_safe_context("HTTP response body exceeded the DNS wire limit"),
        );
    }
    let total = header_end.checked_add(content_length).ok_or_else(|| {
        PortError::new(PortErrorClass::ResourceExhausted, "doh_http_transport.read")
    })?;
    while bytes.len() < total {
        let count = await_io(
            stream.read(&mut chunk),
            deadline,
            cancellation,
            "doh_http_transport.read",
        )
        .await?;
        if count == 0 {
            return Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "doh_http_transport.read",
            )
            .with_safe_context("HTTP response ended before Content-Length"));
        }
        if bytes.len().saturating_add(count) > total {
            return Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "doh_http_transport.read",
            )
            .with_safe_context("HTTP response contained trailing bytes"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }

    let (status, content_type) = parse_status_and_content_type(&bytes[..header_end - 4])?;
    Ok(DohHttpResponseOwned {
        status,
        content_type,
        body: bytes[header_end..total].to_vec(),
    })
}

fn parse_headers(header: &[u8]) -> Result<usize, PortError> {
    let text = std::str::from_utf8(header).map_err(|_| {
        PortError::new(
            PortErrorClass::ProtocolViolation,
            "doh_http_transport.headers",
        )
    })?;
    let mut content_length = None;
    for line in text.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "doh_http_transport.headers",
            ));
        };
        if name.eq_ignore_ascii_case("content-length") {
            let length = value.trim().parse::<usize>().map_err(|_| {
                PortError::new(
                    PortErrorClass::ProtocolViolation,
                    "doh_http_transport.headers",
                )
            })?;
            if content_length.replace(length).is_some() {
                return Err(PortError::new(
                    PortErrorClass::ProtocolViolation,
                    "doh_http_transport.headers",
                ));
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") && !value.trim().is_empty() {
            return Err(PortError::new(
                PortErrorClass::ProtocolViolation,
                "doh_http_transport.headers",
            )
            .with_safe_context("chunked transfer encoding is unsupported"));
        }
    }
    content_length.ok_or_else(|| {
        PortError::new(
            PortErrorClass::ProtocolViolation,
            "doh_http_transport.headers",
        )
        .with_safe_context("Content-Length is required")
    })
}

fn parse_status_and_content_type(header: &[u8]) -> Result<(u16, Option<String>), PortError> {
    let text = std::str::from_utf8(header).map_err(|_| {
        PortError::new(
            PortErrorClass::ProtocolViolation,
            "doh_http_transport.status",
        )
    })?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or_else(|| {
        PortError::new(
            PortErrorClass::ProtocolViolation,
            "doh_http_transport.status",
        )
    })?;
    let mut parts = status_line.split_ascii_whitespace();
    let version = parts.next();
    let status = parts.next().and_then(|value| value.parse::<u16>().ok());
    if !matches!(version, Some("HTTP/1.0" | "HTTP/1.1")) || status.is_none() {
        return Err(PortError::new(
            PortErrorClass::ProtocolViolation,
            "doh_http_transport.status",
        ));
    }
    let status = status.expect("checked above");
    if !(100..=599).contains(&status) {
        return Err(PortError::new(
            PortErrorClass::ProtocolViolation,
            "doh_http_transport.status",
        ));
    }
    let content_type = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-type")
            .then(|| value.trim().to_owned())
    });
    Ok((status, content_type))
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn request_path(endpoint: &Url) -> String {
    let path = if endpoint.path().is_empty() {
        "/"
    } else {
        endpoint.path()
    };
    endpoint
        .query()
        .map_or_else(|| path.to_owned(), |query| format!("{path}?{query}"))
}

fn host_header(endpoint: &Url, host: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    endpoint
        .port()
        .map_or(host.clone(), |port| format!("{host}:{port}"))
}

fn check_budget(
    deadline: Deadline,
    cancellation: &Cancellation,
    operation: &'static str,
) -> Result<(), PortError> {
    if cancellation.is_cancelled() {
        return Err(PortError::new(
            PortErrorClass::Cancelled(
                cancellation
                    .reason()
                    .unwrap_or(CancelReason::UpstreamCancelled),
            ),
            operation,
        ));
    }
    if deadline.is_expired(Instant::now()) {
        return Err(PortError::new(PortErrorClass::Timeout, operation));
    }
    Ok(())
}

async fn await_io<F, T>(
    future: F,
    deadline: Deadline,
    cancellation: &Cancellation,
    operation: &'static str,
) -> Result<T, PortError>
where
    F: std::future::Future<Output = io::Result<T>> + Send,
    T: Send,
{
    check_budget(deadline, cancellation, operation)?;
    tokio::select! {
        result = tokio::time::timeout(deadline.remaining(Instant::now()), future) => {
            match result {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(_)) => Err(PortError::new(PortErrorClass::Unavailable, operation)),
                Err(_) => Err(PortError::new(PortErrorClass::Timeout, operation)),
            }
        }
        _ = cancellation.cancelled() => Err(PortError::new(
            PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled)),
            operation,
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::config::model::OutboundType;
    use crate::config::resolve::{ConfigId, ResolvedOutbound, ResolvedSecretRef};
    use crate::ports::effects::OutboundAddressResolver;
    use crate::upstream::TokioOutboundDialer;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    use crate::ports::{PortError, PortFuture};

    use super::*;

    fn request(address: SocketAddr, body: &[u8]) -> DohHttpRequest {
        DohHttpRequest::new(
            Url::parse(&format!(
                "http://localhost:{}/dns-query?x=1",
                address.port()
            ))
            .unwrap(),
            Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            body.to_vec(),
        )
    }

    fn request_without_connect_ip(address: SocketAddr, body: &[u8]) -> DohHttpRequest {
        DohHttpRequest::new(
            Url::parse(&format!(
                "http://resolver.example.test:{}/dns-query",
                address.port()
            ))
            .unwrap(),
            None,
            body.to_vec(),
        )
    }

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(2))
    }

    struct FakeResolver {
        target: SocketAddr,
        calls: Mutex<Vec<(String, u16)>>,
    }

    impl FakeResolver {
        fn new(target: SocketAddr) -> Self {
            Self {
                target,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl DohAddressResolver for FakeResolver {
        fn resolve<'a>(
            &'a self,
            request: DohAddressRequest,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Vec<SocketAddr>, PortError>> {
            self.calls
                .lock()
                .unwrap()
                .push((request.host().to_owned(), request.port()));
            let target = self.target;
            Box::pin(async move { Ok(vec![target]) })
        }
    }

    struct FakeOutboundResolver {
        target: SocketAddr,
    }

    impl OutboundAddressResolver for FakeOutboundResolver {
        fn resolve<'a>(
            &'a self,
            _host: &'a str,
            _port: u16,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Vec<SocketAddr>, PortError>> {
            let target = self.target;
            Box::pin(async move { Ok(vec![target]) })
        }
    }

    fn proxy_profile(url: &str) -> OutboundProfile {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fluxdns-http-proxy-{}-{timestamp}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("proxy-url");
        fs::write(&path, url).unwrap();
        let outbound = ResolvedOutbound {
            id: ConfigId::new("proxy").unwrap(),
            kind: OutboundType::Socks5,
            proxy_url: ResolvedSecretRef {
                env: None,
                file: Some(path),
            },
        };
        let profile = OutboundProfile::from_resolved(&outbound, 1024).unwrap();
        fs::remove_dir_all(root).unwrap();
        profile
    }

    async fn serve_socks_http(
        expected_connect: Vec<u8>,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();

            let mut connect = vec![0_u8; expected_connect.len()];
            stream.read_exact(&mut connect).await.unwrap();
            assert_eq!(connect, expected_connect);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();

            let mut request = Vec::new();
            let header_end;
            loop {
                let mut chunk = [0_u8; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
                let Some(end) = find_header_end(&request) else {
                    continue;
                };
                if request.len() >= end + 5 {
                    header_end = end + 4;
                    break;
                }
            }
            let request_text = String::from_utf8(request[..header_end].to_vec()).unwrap();
            assert!(request_text.starts_with("POST /dns-query?x=1 HTTP/1.1\r\n"));
            assert!(request_text.contains("Host: dns.example\r\n"));
            assert!(request_text.contains("Content-Length: 5\r\n"));
            assert_eq!(&request[header_end..], b"query");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                )
                .await
                .unwrap();
        });
        (address, server)
    }

    fn proxy_request() -> DohHttpRequest {
        DohHttpRequest::new(
            Url::parse("http://dns.example:80/dns-query?x=1").unwrap(),
            Some(IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 10))),
            b"query".to_vec(),
        )
    }

    fn proxy_request_without_connect_ip() -> DohHttpRequest {
        DohHttpRequest::new(
            Url::parse("http://dns.example/dns-query?x=1").unwrap(),
            None,
            b"query".to_vec(),
        )
    }

    #[tokio::test]
    async fn socks5_http_transport_connects_with_resolved_ip_and_posts_http() {
        let expected_connect = vec![5, 1, 0, 1, 192, 0, 2, 10, 0, 80];
        let (proxy_address, server) = serve_socks_http(expected_connect).await;
        let profile = proxy_profile("socks5://proxy.example");
        let transport = TokioSocks5DohHttpTransport::new(
            profile,
            Arc::new(TokioOutboundDialer::new()),
            Arc::new(FakeOutboundResolver {
                target: proxy_address,
            }),
            Arc::new(TokioDohAddressResolver::new()),
        );

        let response = transport
            .post(proxy_request(), deadline(), &Cancellation::new())
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type.as_deref(), Some(DOH_MEDIA_TYPE));
        assert_eq!(response.body, b"OK");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn socks5h_http_transport_connects_with_target_domain() {
        let expected_connect = vec![
            5, 1, 0, 3, 11, b'd', b'n', b's', b'.', b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0, 80,
        ];
        let (proxy_address, server) = serve_socks_http(expected_connect).await;
        let profile = proxy_profile("socks5h://proxy.example");
        let transport = TokioSocks5DohHttpTransport::new(
            profile,
            Arc::new(TokioOutboundDialer::new()),
            Arc::new(FakeOutboundResolver {
                target: proxy_address,
            }),
            Arc::new(TokioDohAddressResolver::new()),
        );

        let response = transport
            .post(
                proxy_request_without_connect_ip(),
                deadline(),
                &Cancellation::new(),
            )
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"OK");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn posts_with_host_path_and_content_length_and_reads_response() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let header_end = find_header_end(&bytes).unwrap() + 4;
            let request_text = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            assert_eq!(request_text.matches("Content-Length:").count(), 1);
            assert!(request_text.starts_with("POST /dns-query?x=1 HTTP/1.1\r\n"));
            assert!(request_text.contains("Host: localhost:"));
            let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
            stream.write_all(response).await.unwrap();
        });

        let transport = TokioDohHttpTransport::new();
        let response = transport
            .post(request(address, b"abc"), deadline(), &Cancellation::new())
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type.as_deref(), Some(DOH_MEDIA_TYPE));
        assert_eq!(response.body, b"OK");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn uses_injected_address_resolver_when_connect_ip_is_absent() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request_bytes = [0_u8; 512];
            let _ = stream.read(&mut request_bytes).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                )
                .await
                .unwrap();
        });

        let resolver = Arc::new(FakeResolver::new(address));
        let transport = TokioDohHttpTransport::with_resolver(resolver.clone());
        let response = transport
            .post(
                request_without_connect_ip(address, b"abc"),
                deadline(),
                &Cancellation::new(),
            )
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(
            resolver.calls.lock().unwrap().as_slice(),
            &[("resolver.example.test".to_owned(), address.port())]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_connect_ip_bypasses_injected_address_resolver() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request_bytes = [0_u8; 512];
            let _ = stream.read(&mut request_bytes).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                )
                .await
                .unwrap();
        });

        let resolver = Arc::new(FakeResolver::new("192.0.2.99:80".parse().unwrap()));
        let transport = TokioDohHttpTransport::with_resolver(resolver.clone());
        let response = transport
            .post(request(address, b"abc"), deadline(), &Cancellation::new())
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert!(resolver.calls.lock().unwrap().is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn default_resolver_rejects_unconfigured_bootstrap_without_system_fallback() {
        let transport = TokioDohHttpTransport::new();
        let request = DohHttpRequest::new_with_bootstrap(
            Url::parse("http://resolver.example.test/dns-query").unwrap(),
            None,
            Some(crate::config::resolve::ConfigId::new("bootstrap").unwrap()),
            vec![1],
        );
        let error = transport
            .post(request, deadline(), &Cancellation::new())
            .await
            .unwrap_err();

        assert!(matches!(error.class(), PortErrorClass::Unavailable));
        assert_eq!(error.operation(), "doh_http_transport.resolve");
        assert_eq!(
            format!("{error}"),
            "doh_http_transport.resolve failed: unavailable (bootstrap resolver is not configured)"
        );
    }

    #[tokio::test]
    async fn bootstrap_resolver_uses_registered_connector_and_request_port() {
        let registry = Arc::new(crate::upstream::BootstrapConnectorRegistry::default());
        let connector: Arc<dyn crate::ports::exchange::DnsExchange> = Arc::new(
            crate::upstream::HostsExchange::from_resolved(
                &crate::config::resolve::ResolvedUpstream::Hosts {
                    id: crate::config::resolve::ConfigId::new("bootstrap").unwrap(),
                    format: "hosts".to_owned(),
                    hosts: "127.0.0.1 resolver.example.test\n".to_owned(),
                },
            )
            .unwrap(),
        );
        registry.insert(
            crate::config::resolve::ConfigId::new("bootstrap").unwrap(),
            connector.clone(),
        );
        let resolver = TokioDohAddressResolver::with_bootstrap_registry(registry);
        let request = DohAddressRequest::new(
            "resolver.example.test",
            8443,
            Some(crate::config::resolve::ConfigId::new("bootstrap").unwrap()),
        );

        let addresses = resolver
            .resolve(request, deadline(), &Cancellation::new())
            .await
            .unwrap();

        assert_eq!(addresses, vec!["127.0.0.1:8443".parse().unwrap()]);
    }

    #[tokio::test]
    async fn rejects_chunked_responses() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request_bytes = [0_u8; 512];
            let _ = stream.read(&mut request_bytes).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 2\r\n\r\nOK",
                )
                .await
                .unwrap();
        });
        let transport = TokioDohHttpTransport::new();
        let error = transport
            .post(request(address, b"abc"), deadline(), &Cancellation::new())
            .await
            .unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::ProtocolViolation));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn https_requires_a_tls_adapter_and_cancellation_is_observed() {
        let transport = TokioDohHttpTransport::new();
        let request = DohHttpRequest::new(
            Url::parse("https://resolver.example/dns-query").unwrap(),
            None,
            vec![1],
        );
        let error = transport
            .post(request, deadline(), &Cancellation::new())
            .await
            .unwrap_err();
        assert_eq!(error.class().as_str(), "unavailable");

        let cancellation = Cancellation::new();
        cancellation.cancel(CancelReason::ClientDisconnected);
        let request = DohHttpRequest::new(
            Url::parse("http://127.0.0.1/dns-query").unwrap(),
            None,
            vec![1],
        );
        let error = transport
            .post(request, deadline(), &cancellation)
            .await
            .unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::Cancelled(_)));
    }
}
