//! 基于 reqwest 的 HTTP/HTTPS DoH transport adapter。
//!
//! 该 adapter 提供 direct/proxy HTTP/HTTPS 请求边界。每个 adapter 自己持有
//! bounded client pool；pool key 包含目标地址覆盖、proxy endpoint 和 SOCKS
//! 本地/远程解析模式，避免不同 bootstrap/connect_ip 或 proxy 组合复用错误 client。

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use thiserror::Error;
use url::Url;

use crate::config::resolve::ProxyScheme;
use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::OutboundAddressResolver;
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::{
    DOH_MEDIA_TYPE, DohAddressRequest, DohAddressResolver, DohHttpRequest, DohHttpResponseOwned,
    DohHttpTransport, MAX_DOH_RESPONSE_BODY_BYTES, OutboundProfile,
};

#[derive(Debug, Error)]
pub enum ReqwestDohHttpTransportBuildError {
    #[error("reqwest HTTP client could not be built")]
    Client,
    #[error("reqwest proxy could not be configured")]
    Proxy,
}

#[derive(Clone)]
pub struct ReqwestDohHttpTransport {
    pool: Arc<ClientPool>,
    resolver: Arc<dyn DohAddressResolver>,
    proxy: Option<OutboundProfile>,
    proxy_resolver: Option<Arc<dyn OutboundAddressResolver>>,
    root_certificate: Option<reqwest::Certificate>,
}

impl std::fmt::Debug for ReqwestDohHttpTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReqwestDohHttpTransport")
            .field("has_resolver", &true)
            .field("has_proxy", &self.proxy.is_some())
            .field("pool_size", &self.pool.len())
            .finish()
    }
}

impl ReqwestDohHttpTransport {
    pub fn new(
        resolver: Arc<dyn DohAddressResolver>,
    ) -> Result<Self, ReqwestDohHttpTransportBuildError> {
        Self::build(resolver, None, None, None)
    }

    pub fn with_proxy(
        resolver: Arc<dyn DohAddressResolver>,
        proxy_resolver: Arc<dyn OutboundAddressResolver>,
        profile: OutboundProfile,
    ) -> Result<Self, ReqwestDohHttpTransportBuildError> {
        Self::build(resolver, Some(profile), Some(proxy_resolver), None)
    }

    fn build(
        resolver: Arc<dyn DohAddressResolver>,
        proxy: Option<OutboundProfile>,
        proxy_resolver: Option<Arc<dyn OutboundAddressResolver>>,
        root_certificate: Option<reqwest::Certificate>,
    ) -> Result<Self, ReqwestDohHttpTransportBuildError> {
        client_builder()
            .build()
            .map_err(|_| ReqwestDohHttpTransportBuildError::Client)?;
        if let Some(profile) = proxy.as_ref() {
            let text = std::str::from_utf8(profile.proxy_url().expose())
                .map_err(|_| ReqwestDohHttpTransportBuildError::Proxy)?;
            reqwest::Proxy::all(text).map_err(|_| ReqwestDohHttpTransportBuildError::Proxy)?;
        }
        Ok(Self {
            pool: Arc::new(ClientPool::new()),
            resolver,
            proxy,
            proxy_resolver,
            root_certificate,
        })
    }

    #[cfg(test)]
    fn with_test_root_certificate(
        resolver: Arc<dyn DohAddressResolver>,
        der: &[u8],
    ) -> Result<Self, ReqwestDohHttpTransportBuildError> {
        let certificate = reqwest::Certificate::from_der(der)
            .map_err(|_| ReqwestDohHttpTransportBuildError::Client)?;
        Self::build(resolver, None, None, Some(certificate))
    }

    async fn client_for_request(
        &self,
        request: &DohHttpRequest,
        deadline: Deadline,
        cancellation: &Cancellation,
    ) -> Result<reqwest::Client, PortError> {
        let Some(addresses) =
            request_addresses(request, &*self.resolver, deadline, cancellation).await?
        else {
            let addresses = Vec::new();
            let proxy_address = self.resolve_proxy_address(deadline, cancellation).await?;
            let key = ClientPoolKey::new(
                request,
                addresses.clone(),
                proxy_address,
                self.proxy_remote_dns(request),
            );
            return self.client_for_key(
                request,
                key,
                addresses,
                proxy_address,
                deadline,
                cancellation,
            );
        };
        let proxy_address = self.resolve_proxy_address(deadline, cancellation).await?;
        let key = ClientPoolKey::new(
            request,
            addresses.clone(),
            proxy_address,
            self.proxy_remote_dns(request),
        );
        self.client_for_key(
            request,
            key,
            addresses,
            proxy_address,
            deadline,
            cancellation,
        )
    }

    fn client_for_key(
        &self,
        request: &DohHttpRequest,
        key: ClientPoolKey,
        addresses: Vec<SocketAddr>,
        proxy_address: Option<SocketAddr>,
        _deadline: Deadline,
        _cancellation: &Cancellation,
    ) -> Result<reqwest::Client, PortError> {
        if let Some(client) = self.pool.get(&key) {
            return Ok(client);
        }
        let client = self.build_client(request, &addresses, proxy_address)?;
        self.pool.insert(key, client.clone());
        Ok(client)
    }

    async fn resolve_proxy_address(
        &self,
        deadline: Deadline,
        cancellation: &Cancellation,
    ) -> Result<Option<SocketAddr>, PortError> {
        let Some(profile) = self.proxy.as_ref() else {
            return Ok(None);
        };
        if let Ok(ip) = profile.proxy_host().parse::<IpAddr>() {
            return Ok(Some(SocketAddr::new(ip, profile.proxy_port())));
        }
        let resolver = self.proxy_resolver.as_ref().ok_or_else(|| {
            PortError::new(
                PortErrorClass::Internal,
                "reqwest_doh_transport.proxy_resolve",
            )
            .with_safe_context("proxy resolver is not configured")
        })?;
        let addresses = resolver
            .resolve(
                profile.proxy_host(),
                profile.proxy_port(),
                deadline,
                cancellation,
            )
            .await?;
        addresses
            .into_iter()
            .next()
            .ok_or_else(|| {
                PortError::new(
                    PortErrorClass::Unavailable,
                    "reqwest_doh_transport.proxy_resolve",
                )
                .with_safe_context("proxy resolver returned no addresses")
            })
            .map(Some)
    }

    fn proxy_remote_dns(&self, request: &DohHttpRequest) -> bool {
        self.proxy.as_ref().is_some_and(|profile| {
            matches!(profile.scheme(), ProxyScheme::Socks5h) && request.connect_ip().is_none()
        })
    }

    fn build_client(
        &self,
        request: &DohHttpRequest,
        addresses: &[SocketAddr],
        proxy_address: Option<SocketAddr>,
    ) -> Result<reqwest::Client, PortError> {
        let mut builder = client_builder();
        if !addresses.is_empty() {
            builder = builder.resolve_to_addrs(request.host(), addresses);
        }
        if let (Some(profile), Some(proxy_address)) = (&self.proxy, proxy_address) {
            let proxy_url =
                proxy_url_for_request(profile, proxy_address, self.proxy_remote_dns(request))?;
            let proxy = reqwest::Proxy::all(proxy_url).map_err(|_| {
                PortError::new(PortErrorClass::Internal, "reqwest_doh_transport.proxy")
                    .with_safe_context("reqwest proxy could not be configured")
            })?;
            builder = builder.proxy(proxy);
        }
        if let Some(certificate) = &self.root_certificate {
            builder = builder.add_root_certificate(certificate.clone());
        }
        builder.build().map_err(|_| {
            PortError::new(PortErrorClass::Internal, "reqwest_doh_transport.client")
                .with_safe_context("reqwest client could not be built")
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ClientPoolKey {
    target_host: Option<String>,
    target_port: Option<u16>,
    target_https: bool,
    addresses: Vec<SocketAddr>,
    proxy_address: Option<SocketAddr>,
    proxy_remote_dns: bool,
}

impl ClientPoolKey {
    fn new(
        request: &DohHttpRequest,
        addresses: Vec<SocketAddr>,
        proxy_address: Option<SocketAddr>,
        proxy_remote_dns: bool,
    ) -> Self {
        Self {
            target_host: (!addresses.is_empty()).then(|| request.host().to_owned()),
            target_port: (!addresses.is_empty()).then(|| {
                request.endpoint().port().unwrap_or_else(|| {
                    if request.endpoint().scheme() == "https" {
                        443
                    } else {
                        80
                    }
                })
            }),
            target_https: request.endpoint().scheme() == "https",
            addresses,
            proxy_address,
            proxy_remote_dns,
        }
    }
}

struct ClientPool {
    state: Mutex<ClientPoolState>,
}

struct ClientPoolState {
    clients: HashMap<ClientPoolKey, reqwest::Client>,
    order: VecDeque<ClientPoolKey>,
}

impl ClientPool {
    const MAX_CLIENTS: usize = 32;

    fn new() -> Self {
        Self {
            state: Mutex::new(ClientPoolState {
                clients: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    fn get(&self, key: &ClientPoolKey) -> Option<reqwest::Client> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let client = state.clients.get(key).cloned();
        if client.is_some() {
            state.order.retain(|item| item != key);
            state.order.push_back(key.clone());
        }
        client
    }

    fn insert(&self, key: ClientPoolKey, client: reqwest::Client) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clients.insert(key.clone(), client);
        state.order.retain(|item| item != &key);
        state.order.push_back(key);
        while state.clients.len() > Self::MAX_CLIENTS {
            let Some(oldest) = state.order.pop_front() else {
                break;
            };
            state.clients.remove(&oldest);
        }
    }

    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clients
            .len()
    }
}

fn proxy_url_for_request(
    profile: &OutboundProfile,
    proxy_address: SocketAddr,
    remote_dns: bool,
) -> Result<String, PortError> {
    let text = std::str::from_utf8(profile.proxy_url().expose()).map_err(|_| {
        PortError::new(PortErrorClass::Internal, "reqwest_doh_transport.proxy")
            .with_safe_context("proxy URL is not valid UTF-8")
    })?;
    let mut url = Url::parse(text).map_err(|_| {
        PortError::new(PortErrorClass::Internal, "reqwest_doh_transport.proxy")
            .with_safe_context("proxy URL could not be parsed")
    })?;
    if matches!(profile.scheme(), ProxyScheme::Socks5h) && !remote_dns {
        url.set_scheme("socks5").map_err(|_| {
            PortError::new(PortErrorClass::Internal, "reqwest_doh_transport.proxy")
                .with_safe_context("proxy scheme could not be adjusted")
        })?;
    }
    url.set_host(Some(&proxy_address.ip().to_string()))
        .map_err(|_| {
            PortError::new(PortErrorClass::Internal, "reqwest_doh_transport.proxy")
                .with_safe_context("proxy host could not be adjusted")
        })?;
    url.set_port(Some(proxy_address.port())).map_err(|_| {
        PortError::new(PortErrorClass::Internal, "reqwest_doh_transport.proxy")
            .with_safe_context("proxy port could not be adjusted")
    })?;
    Ok(url.to_string())
}

impl DohHttpTransport for ReqwestDohHttpTransport {
    fn post<'a>(
        &'a self,
        request: DohHttpRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
        Box::pin(async move {
            check_budget(deadline, cancellation, "reqwest_doh_transport.post")?;
            let client = self
                .client_for_request(&request, deadline, cancellation)
                .await?;
            let response = send_request(&client, &request, deadline, cancellation).await?;
            read_response(response, deadline, cancellation).await
        })
    }
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .use_rustls_tls()
}

async fn request_addresses(
    request: &DohHttpRequest,
    resolver: &dyn DohAddressResolver,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<Option<Vec<SocketAddr>>, PortError> {
    let port = request.endpoint().port().unwrap_or_else(|| {
        if request.endpoint().scheme() == "https" {
            443
        } else {
            80
        }
    });
    if let Some(connect_ip) = request.connect_ip() {
        return Ok(Some(vec![SocketAddr::new(connect_ip, port)]));
    }
    if request.bootstrap().is_none() {
        return Ok(None);
    }
    let addresses = resolver
        .resolve(
            DohAddressRequest::new(request.host(), port, request.bootstrap().cloned()),
            deadline,
            cancellation,
        )
        .await?;
    if addresses.is_empty() {
        return Err(
            PortError::new(PortErrorClass::Unavailable, "reqwest_doh_transport.resolve")
                .with_safe_context("bootstrap resolver returned no addresses"),
        );
    }
    Ok(Some(addresses))
}

async fn send_request(
    client: &reqwest::Client,
    request: &DohHttpRequest,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<reqwest::Response, PortError> {
    let future = client
        .post(request.endpoint().as_str())
        .header(CONTENT_TYPE, DOH_MEDIA_TYPE)
        .header(ACCEPT, DOH_MEDIA_TYPE)
        .body(request.body().to_vec())
        .send();
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(PortError::new(
            PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled)),
            "reqwest_doh_transport.send",
        )),
        result = tokio::time::timeout(deadline.remaining(Instant::now()), future) => {
            match result {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(error)) => Err(map_reqwest_error(error, "reqwest_doh_transport.send")),
                Err(_) => Err(PortError::new(PortErrorClass::Timeout, "reqwest_doh_transport.send")),
            }
        }
    }
}

async fn read_response(
    mut response: reqwest::Response,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<DohHttpResponseOwned, PortError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOH_RESPONSE_BODY_BYTES as u64)
    {
        return Err(PortError::new(
            PortErrorClass::ResourceExhausted,
            "reqwest_doh_transport.read",
        ));
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_DOH_RESPONSE_BODY_BYTES as u64) as usize,
    );
    loop {
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(PortError::new(
                    PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled)),
                    "reqwest_doh_transport.read",
                ));
            }
            result = tokio::time::timeout(deadline.remaining(Instant::now()), response.chunk()) => {
                match result {
                    Ok(Ok(chunk)) => chunk,
                    Ok(Err(error)) => return Err(map_reqwest_error(error, "reqwest_doh_transport.read")),
                    Err(_) => return Err(PortError::new(PortErrorClass::Timeout, "reqwest_doh_transport.read")),
                }
            }
        };
        let Some(chunk) = next else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > MAX_DOH_RESPONSE_BODY_BYTES {
            return Err(PortError::new(
                PortErrorClass::ResourceExhausted,
                "reqwest_doh_transport.read",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    Ok(DohHttpResponseOwned {
        status: response.status().as_u16(),
        content_type,
        body,
    })
}

fn map_reqwest_error(error: reqwest::Error, operation: &'static str) -> PortError {
    if error.is_timeout() {
        return PortError::new(PortErrorClass::Timeout, operation)
            .with_safe_context("reqwest HTTP request timed out");
    }
    if error.is_connect() {
        return PortError::new(PortErrorClass::Unavailable, operation)
            .with_safe_context("reqwest HTTP connection failed");
    }
    if error.is_body() {
        return PortError::new(PortErrorClass::Unavailable, operation)
            .with_safe_context("reqwest HTTP body read failed");
    }
    PortError::new(PortErrorClass::Internal, operation)
        .with_safe_context("reqwest HTTP request failed")
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use url::Url;

    use super::*;
    use crate::config::resolve::ConfigId;
    use crate::config::resolve::{ResolvedOutbound, ResolvedSecretRef};
    use crate::upstream::TokioDohAddressResolver;

    async fn read_request<S>(stream: &mut S) -> (String, Vec<u8>)
    where
        S: AsyncRead + Unpin,
    {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let body_start = header_end + 4;
        while bytes.len() < body_start + content_length {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        (
            headers,
            bytes[body_start..body_start + content_length].to_vec(),
        )
    }

    struct Resolver {
        address: SocketAddr,
        calls: Mutex<usize>,
    }

    impl Resolver {
        fn new(address: SocketAddr) -> Self {
            Self {
                address,
                calls: Mutex::new(0),
            }
        }
    }

    impl DohAddressResolver for Resolver {
        fn resolve<'a>(
            &'a self,
            _request: DohAddressRequest,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Vec<SocketAddr>, PortError>> {
            *self.calls.lock().unwrap() += 1;
            let address = self.address;
            Box::pin(async move { Ok(vec![address]) })
        }
    }

    fn request(
        endpoint: Url,
        connect_ip: Option<IpAddr>,
        bootstrap: Option<ConfigId>,
    ) -> DohHttpRequest {
        let body = vec![1_u8, 2, 3, 4];
        match bootstrap {
            Some(bootstrap) => {
                DohHttpRequest::new_with_bootstrap(endpoint, connect_ip, Some(bootstrap), body)
            }
            None => DohHttpRequest::new(endpoint, connect_ip, body),
        }
    }

    fn proxy_profile(url: &str) -> (OutboundProfile, std::path::PathBuf) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "fluxdns-reqwest-proxy-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("proxy-url");
        fs::write(&path, url).unwrap();
        let outbound = ResolvedOutbound {
            id: ConfigId::new("proxy").unwrap(),
            kind: crate::config::model::OutboundType::Socks5,
            proxy_url: ResolvedSecretRef {
                env: None,
                file: Some(path),
            },
        };
        let profile = OutboundProfile::from_resolved(&outbound, 1024).unwrap();
        (profile, root)
    }

    #[tokio::test]
    async fn posts_with_connect_ip_and_preserves_http_envelope() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, body) = read_request(&mut stream).await;
            let headers = headers.to_ascii_lowercase();
            assert!(headers.contains("post /dns-query http/1.1"));
            assert!(headers.contains(&format!("host: resolver.example.test:{}", address.port())));
            assert!(headers.contains("content-type: application/dns-message"));
            assert_eq!(body, [1, 2, 3, 4]);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
                )
                .await
                .unwrap();
        });

        let transport =
            ReqwestDohHttpTransport::new(Arc::new(TokioDohAddressResolver::new())).unwrap();
        let request = request(
            Url::parse(&format!(
                "http://resolver.example.test:{}/dns-query",
                address.port()
            ))
            .unwrap(),
            Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            None,
        );
        let cancellation = Cancellation::new();
        let response = transport
            .post(
                request,
                Deadline::new(Instant::now() + Duration::from_secs(2)),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type.as_deref(), Some(DOH_MEDIA_TYPE));
        assert_eq!(response.body, b"abc");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn performs_live_https_tls_handshake_with_verified_host() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["resolver.example.test".to_owned()]).unwrap();
        let certificate_der = certified.cert.der().to_vec();
        let private_key_der = certified.signing_key.serialize_der();
        let mut server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_der)),
            )
            .unwrap();
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            let (headers, body) = read_request(&mut stream).await;
            let headers = headers.to_ascii_lowercase();
            assert!(headers.contains("post /dns-query http/1.1"));
            assert!(headers.contains(&format!("host: resolver.example.test:{}", address.port())));
            assert_eq!(body, [1, 2, 3, 4]);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
                )
                .await
                .unwrap();
        });

        let transport = ReqwestDohHttpTransport::with_test_root_certificate(
            Arc::new(TokioDohAddressResolver::new()),
            &certificate_der,
        )
        .unwrap();
        let request = request(
            Url::parse(&format!(
                "https://resolver.example.test:{}/dns-query",
                address.port()
            ))
            .unwrap(),
            Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            None,
        );
        let cancellation = Cancellation::new();
        let response = transport
            .post(
                request,
                // Windows 本地 Rustls 冷启动可能超过 2s；该预算只用于握手测试，不改变生产配置。
                Deadline::new(Instant::now() + Duration::from_secs(10)),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type.as_deref(), Some(DOH_MEDIA_TYPE));
        assert_eq!(response.body, b"abc");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bootstrap_override_calls_injected_resolver() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Type: application/dns-message\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let resolver = Arc::new(Resolver::new(address));
        let transport = ReqwestDohHttpTransport::new(resolver.clone()).unwrap();
        let request = request(
            Url::parse(&format!(
                "http://resolver.example.test:{}/dns-query",
                address.port()
            ))
            .unwrap(),
            None,
            Some(ConfigId::new("bootstrap").unwrap()),
        );
        let cancellation = Cancellation::new();
        transport
            .post(
                request,
                Deadline::new(Instant::now() + Duration::from_secs(2)),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(*resolver.calls.lock().unwrap(), 1);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_is_reported_before_client_creation() {
        let transport =
            ReqwestDohHttpTransport::new(Arc::new(TokioDohAddressResolver::new())).unwrap();
        let request = request(
            Url::parse("https://resolver.example.test/dns-query").unwrap(),
            None,
            None,
        );
        let cancellation = Cancellation::new();
        cancellation.cancel(CancelReason::ClientDisconnected);
        let error = transport
            .post(
                request,
                Deadline::new(Instant::now() + Duration::from_secs(2)),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::Cancelled(_)));
    }

    #[tokio::test]
    async fn socks5h_proxy_with_connect_ip_uses_ip_target_and_reuses_pool_entry() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();

            let mut connect = [0_u8; 10];
            stream.read_exact(&mut connect).await.unwrap();
            assert_eq!(connect, [5, 1, 0, 1, 127, 0, 0, 1, 0, 80]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();

            let (headers, body) = read_request(&mut stream).await;
            let headers = headers.to_ascii_lowercase();
            assert!(headers.contains("post /dns-query http/1.1"));
            assert!(headers.contains("host: resolver.example.test"));
            assert_eq!(body, [1, 2, 3, 4]);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
                )
                .await
                .unwrap();
        });

        let (profile, root) = proxy_profile(&format!("socks5h://{proxy_address}"));
        let transport = ReqwestDohHttpTransport::with_proxy(
            Arc::new(TokioDohAddressResolver::new()),
            Arc::new(crate::upstream::TokioOutboundAddressResolver::new()),
            profile,
        )
        .unwrap();
        let request = request(
            Url::parse("http://resolver.example.test/dns-query").unwrap(),
            Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            None,
        );
        let cancellation = Cancellation::new();
        let _ = transport
            .client_for_request(
                &request,
                Deadline::new(Instant::now() + Duration::from_secs(2)),
                &cancellation,
            )
            .await
            .unwrap();
        let response = transport
            .post(
                request,
                Deadline::new(Instant::now() + Duration::from_secs(2)),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(response.body, b"abc");
        assert_eq!(transport.pool.len(), 1);
        server.await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
