//! 基于 Tokio TCP 的 plain HTTP/1.1 DoH transport adapter。
//!
//! 该 adapter 只负责 `http://` endpoint 的一次请求/响应交换；HTTPS/TLS、proxy
//! 和连接池由后续 adapter 提供。所有读取均受 header/body/deadline 上限约束。

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, lookup_host};
use url::Url;

use crate::config::resolve::ConfigId;
use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::{
    BootstrapConnectorRegistry, BootstrapResolver, BootstrapResolverError, DOH_MEDIA_TYPE,
    DohHttpRequest, DohHttpResponseOwned, DohHttpTransport,
};

const MAX_HTTP_HEADER_BYTES: usize = 32 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;

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
    fn new(host: &str, port: u16, bootstrap: Option<ConfigId>) -> Self {
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
/// `connect_ip` 由调用方显式提供时不会调用该 port；后续 bootstrap
/// resolver 可以在不改变 HTTP/DNS 协议边界的情况下替换默认实现。
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
}

impl std::fmt::Debug for TokioDohAddressResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokioDohAddressResolver")
            .field("has_bootstrap_registry", &self.bootstrap.is_some())
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
        Self { bootstrap: None }
    }

    pub(crate) fn with_bootstrap_registry(bootstrap: Arc<BootstrapConnectorRegistry>) -> Self {
        Self {
            bootstrap: Some(bootstrap),
        }
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
            if let Some(bootstrap_id) = request.bootstrap() {
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
                let answer = BootstrapResolver::new(connector)
                    .resolve_with_budget(request.host(), deadline, cancellation)
                    .await
                    .map_err(map_bootstrap_error)?;
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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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
