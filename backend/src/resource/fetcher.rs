//! 基于 reqwest 的 remote resource fetcher。
//!
//! 该 adapter 在 prepare 边界读取并解析 outbound SecretRef，运行时只接受已构造的
//! `ResourceFetchRequest`。它不跟随重定向、不继承环境代理，并按请求提供的上限读取 body。

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;
use url::Url;

use crate::config::migrate::deterministic_hash;
use crate::config::resolve::ResolvedOutbound;
use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::{
    ProxyProfileId, ResourceContent, ResourceFetchRequest, ResourceFetchResult, ResourceFetcher,
    ResourceValidators,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};
use crate::upstream::{OutboundProfile, OutboundProfileError};

#[derive(Debug, Error)]
pub enum ResourceFetcherBuildError {
    #[error("outbound `{outbound}` could not build a resource proxy profile: {source}")]
    InvalidOutbound {
        outbound: String,
        #[source]
        source: OutboundProfileError,
    },
    #[error("outbound `{outbound}` is duplicated")]
    DuplicateOutbound { outbound: String },
    #[error("resource HTTP client could not be built")]
    Client,
    #[error("resource proxy client could not be built")]
    Proxy,
    #[error("resource validator scope could not be initialized")]
    ValidatorScope,
}

/// 使用已解析 outbound profile 的有界 remote resource fetcher。
pub struct ReqwestResourceFetcher {
    direct: reqwest::Client,
    proxies: HashMap<String, reqwest::Client>,
    validator_scope: String,
}

impl fmt::Debug for ReqwestResourceFetcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReqwestResourceFetcher")
            .field("proxy_profile_count", &self.proxies.len())
            .finish()
    }
}

impl ReqwestResourceFetcher {
    pub fn from_resolved(
        outbounds: &[ResolvedOutbound],
        max_secret_bytes: usize,
    ) -> Result<Self, ResourceFetcherBuildError> {
        Self::build(outbounds, max_secret_bytes, None)
    }

    #[cfg(test)]
    fn with_test_root_certificate(
        outbounds: &[ResolvedOutbound],
        max_secret_bytes: usize,
        der: &[u8],
    ) -> Result<Self, ResourceFetcherBuildError> {
        let certificate =
            reqwest::Certificate::from_der(der).map_err(|_| ResourceFetcherBuildError::Client)?;
        Self::build(outbounds, max_secret_bytes, Some(certificate))
    }

    fn build(
        outbounds: &[ResolvedOutbound],
        max_secret_bytes: usize,
        root_certificate: Option<reqwest::Certificate>,
    ) -> Result<Self, ResourceFetcherBuildError> {
        let direct = build_client(None, root_certificate.clone())?;
        let mut proxies = HashMap::new();
        for outbound in outbounds {
            if proxies.contains_key(outbound.id.as_str()) {
                return Err(ResourceFetcherBuildError::DuplicateOutbound {
                    outbound: outbound.id.as_str().to_owned(),
                });
            }
            let profile =
                OutboundProfile::from_resolved(outbound, max_secret_bytes).map_err(|source| {
                    ResourceFetcherBuildError::InvalidOutbound {
                        outbound: outbound.id.as_str().to_owned(),
                        source,
                    }
                })?;
            let client = build_client(Some(&profile), root_certificate.clone())?;
            proxies.insert(outbound.id.as_str().to_owned(), client);
        }
        let mut scope = [0_u8; 16];
        getrandom::fill(&mut scope).map_err(|_| ResourceFetcherBuildError::ValidatorScope)?;
        let validator_scope = scope.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self {
            direct,
            proxies,
            validator_scope,
        })
    }

    fn client_for(&self, profile: Option<&ProxyProfileId>) -> Result<reqwest::Client, PortError> {
        match profile {
            None => Ok(self.direct.clone()),
            Some(profile) => self
                .proxies
                .get(profile.0.as_ref())
                .cloned()
                .ok_or_else(|| {
                    PortError::new(PortErrorClass::InvalidInput, "resource_fetch.proxy")
                        .with_safe_context("resource proxy profile is not registered")
                }),
        }
    }
}

impl ResourceFetcher for ReqwestResourceFetcher {
    fn validator_scope(&self) -> Option<&str> {
        Some(&self.validator_scope)
    }

    fn fetch<'a>(
        &'a self,
        request: ResourceFetchRequest,
    ) -> PortFuture<'a, Result<ResourceFetchResult, PortError>> {
        let client = match self.client_for(request.proxy_profile.as_ref()) {
            Ok(client) => client,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        Box::pin(async move { fetch_with_client(&client, request).await })
    }
}

fn build_client(
    profile: Option<&OutboundProfile>,
    root_certificate: Option<reqwest::Certificate>,
) -> Result<reqwest::Client, ResourceFetcherBuildError> {
    crate::ensure_rustls_crypto_provider();
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .use_rustls_tls();
    if let Some(profile) = profile {
        let proxy_url = std::str::from_utf8(profile.proxy_url().expose())
            .map_err(|_| ResourceFetcherBuildError::Proxy)?;
        let proxy = reqwest::Proxy::all(proxy_url).map_err(|_| ResourceFetcherBuildError::Proxy)?;
        builder = builder.proxy(proxy);
    }
    if let Some(certificate) = root_certificate {
        builder = builder.add_root_certificate(certificate);
    }
    builder
        .build()
        .map_err(|_| ResourceFetcherBuildError::Client)
}

async fn fetch_with_client(
    client: &reqwest::Client,
    request: ResourceFetchRequest,
) -> Result<ResourceFetchResult, PortError> {
    let url = Url::parse(request.location.as_str()).map_err(|_| {
        PortError::new(PortErrorClass::InvalidInput, "resource_fetch.url")
            .with_safe_context("resource URL could not be parsed")
    })?;
    if request.max_bytes == 0 {
        return Err(
            PortError::new(PortErrorClass::InvalidInput, "resource_fetch.read")
                .with_safe_context("resource body limit must be greater than zero"),
        );
    }
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(
            PortError::new(PortErrorClass::InvalidInput, "resource_fetch.url")
                .with_safe_context("resource URL does not satisfy the safe HTTP boundary"),
        );
    }
    check_budget(
        &request.deadline,
        &request.cancellation,
        "resource_fetch.send",
    )?;

    request.validators.validate()?;
    let mut builder = client.get(url);
    if let Some(etag) = &request.validators.etag {
        builder = builder.header(reqwest::header::IF_NONE_MATCH, etag.as_ref());
    }
    if let Some(modified) = &request.validators.last_modified {
        builder = builder.header(reqwest::header::IF_MODIFIED_SINCE, modified.as_ref());
    }
    let response = tokio::select! {
        biased;
        _ = request.cancellation.cancelled() => {
            return Err(cancelled_error(&request.cancellation, "resource_fetch.send"));
        }
        result = tokio::time::timeout(request.deadline.remaining(Instant::now()), builder.send()) => {
            match result {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(map_reqwest_error(error, "resource_fetch.send")),
                Err(_) => return Err(PortError::new(PortErrorClass::Timeout, "resource_fetch.send")),
            }
        }
    };

    let validators = ResourceValidators {
        etag: response_header(&response, reqwest::header::ETAG)?,
        last_modified: response_header(&response, reqwest::header::LAST_MODIFIED)?,
    };
    validators.validate()?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(ResourceFetchResult::NotModified(validators));
    }
    if !response.status().is_success() {
        return Err(
            PortError::new(PortErrorClass::ProtocolViolation, "resource_fetch.status")
                .with_safe_context("resource response status is not successful"),
        );
    }
    if response
        .content_length()
        .is_some_and(|length| length > request.max_bytes as u64)
    {
        return Err(PortError::new(
            PortErrorClass::ResourceExhausted,
            "resource_fetch.read",
        ));
    }

    let modified_at = None;
    let mut response = response;
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(request.max_bytes as u64) as usize,
    );
    loop {
        let next = tokio::select! {
            biased;
            _ = request.cancellation.cancelled() => {
                return Err(cancelled_error(&request.cancellation, "resource_fetch.read"));
            }
            result = tokio::time::timeout(request.deadline.remaining(Instant::now()), response.chunk()) => {
                match result {
                    Ok(Ok(chunk)) => chunk,
                    Ok(Err(error)) => return Err(map_reqwest_error(error, "resource_fetch.read")),
                    Err(_) => return Err(PortError::new(PortErrorClass::Timeout, "resource_fetch.read")),
                }
            }
        };
        let Some(chunk) = next else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > request.max_bytes {
            return Err(PortError::new(
                PortErrorClass::ResourceExhausted,
                "resource_fetch.read",
            ));
        }
        body.extend_from_slice(&chunk);
    }

    let checksum = u64::from_str_radix(&deterministic_hash(&body), 16).unwrap_or_default();
    Ok(ResourceFetchResult::Modified(ResourceContent {
        body: Arc::from(body.into_boxed_slice()),
        checksum,
        modified_at,
        validators,
    }))
}

fn response_header(
    response: &reqwest::Response,
    name: reqwest::header::HeaderName,
) -> Result<Option<Arc<str>>, PortError> {
    response
        .headers()
        .get(name)
        .map(|value| {
            value.to_str().map(Arc::from).map_err(|_| {
                PortError::new(
                    PortErrorClass::ProtocolViolation,
                    "resource_fetch.validator",
                )
            })
        })
        .transpose()
}

fn check_budget(
    deadline: &Deadline,
    cancellation: &Cancellation,
    operation: &'static str,
) -> Result<(), PortError> {
    if cancellation.is_cancelled() {
        return Err(cancelled_error(cancellation, operation));
    }
    if deadline.is_expired(Instant::now()) {
        return Err(PortError::new(PortErrorClass::Timeout, operation));
    }
    Ok(())
}

fn cancelled_error(cancellation: &Cancellation, operation: &'static str) -> PortError {
    PortError::new(
        PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::Shutdown)),
        operation,
    )
}

fn map_reqwest_error(error: reqwest::Error, operation: &'static str) -> PortError {
    if error.is_timeout() {
        return PortError::new(PortErrorClass::Timeout, operation)
            .with_safe_context("resource HTTP request timed out");
    }
    if error.is_connect() {
        return PortError::new(PortErrorClass::Unavailable, operation)
            .with_safe_context("resource HTTP connection failed");
    }
    if error.is_body() {
        return PortError::new(PortErrorClass::Unavailable, operation)
            .with_safe_context("resource HTTP body read failed");
    }
    PortError::new(PortErrorClass::Internal, operation)
        .with_safe_context("resource HTTP request failed")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    use super::*;
    use crate::config::model::OutboundType;
    use crate::config::resolve::{ConfigId, ResolvedOutbound, ResolvedSecretRef};
    use crate::dns::{Cancellation, Deadline};
    use crate::ports::effects::ResourceLocation;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn request(url: &str, max_bytes: usize) -> ResourceFetchRequest {
        ResourceFetchRequest {
            location: ResourceLocation::new(Arc::<str>::from(url)).unwrap(),
            proxy_profile: None,
            max_bytes,
            deadline: Deadline::new(Instant::now() + Duration::from_secs(2)),
            cancellation: Cancellation::new(),
            validators: ResourceValidators::default(),
        }
    }

    async fn server(
        body: &'static [u8],
        status: &'static str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        (address, task)
    }

    async fn read_headers<S>(stream: &mut S) -> String
    where
        S: AsyncRead + Unpin,
    {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                return String::from_utf8(bytes[..index].to_vec()).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn conditional_headers_and_304_use_typed_results_without_a_body() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for conditional in [false, true] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let headers = read_headers(&mut stream).await.to_ascii_lowercase();
                if conditional {
                    assert!(headers.contains("if-none-match: \"version-one\""));
                    assert!(headers.contains("if-modified-since: sat, 05 sep 2026 00:00:00 gmt"));
                    stream.write_all(b"HTTP/1.1 304 Not Modified\r\nETag: \"version-two\"\r\nConnection: close\r\n\r\n").await.unwrap();
                } else {
                    assert!(!headers.contains("if-none-match"));
                    stream.write_all(b"HTTP/1.1 200 OK\r\nETag: \"version-one\"\r\nLast-Modified: Sat, 05 Sep 2026 00:00:00 GMT\r\nContent-Length: 4\r\nConnection: close\r\n\r\nrule").await.unwrap();
                }
            }
        });
        let fetcher = ReqwestResourceFetcher::from_resolved(&[], 1024).unwrap();
        let mut request = request(&format!("http://{address}/rules"), 1024);
        let ResourceFetchResult::Modified(content) = fetcher.fetch(request.clone()).await.unwrap()
        else {
            panic!("expected body")
        };
        assert_eq!(content.body.as_ref(), b"rule");
        request.validators = content.validators;
        let ResourceFetchResult::NotModified(validators) = fetcher.fetch(request).await.unwrap()
        else {
            panic!("expected 304")
        };
        assert_eq!(validators.etag.as_deref(), Some("\"version-two\""));
        task.await.unwrap();
        assert_ne!(
            fetcher.validator_scope(),
            ReqwestResourceFetcher::from_resolved(&[], 1024)
                .unwrap()
                .validator_scope()
        );
    }

    #[tokio::test]
    async fn invalid_validators_are_rejected_before_network() {
        let fetcher = ReqwestResourceFetcher::from_resolved(&[], 1024).unwrap();
        for etag in ["bad\r\nInjected: secret".to_owned(), "x".repeat(4097)] {
            let mut request = request("http://127.0.0.1:9/rules", 1024);
            request.validators.etag = Some(Arc::from(etag));
            let error = fetcher.fetch(request).await.unwrap_err();
            assert_eq!(error.operation(), "resource_fetch.validator");
        }
    }

    fn proxy_outbound(url: &str) -> (ResolvedOutbound, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-resource-fetcher-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("proxy-url");
        std::fs::write(&path, url).unwrap();
        (
            ResolvedOutbound {
                id: ConfigId::new("proxy").unwrap(),
                kind: OutboundType::Socks5,
                proxy_url: ResolvedSecretRef {
                    env: None,
                    file: Some(path),
                },
            },
            root,
        )
    }

    #[tokio::test]
    async fn fetches_bounded_http_body_and_computes_stable_checksum() {
        let (address, task) = server(b"DOMAIN-SUFFIX,example.test\n", "200 OK").await;
        let fetcher = ReqwestResourceFetcher::from_resolved(&[], 1024).unwrap();
        let result = fetcher
            .fetch(request(
                &format!("http://127.0.0.1:{}/rules", address.port()),
                1024,
            ))
            .await
            .unwrap();
        let ResourceFetchResult::Modified(result) = result else {
            panic!("expected body")
        };
        assert_eq!(&*result.body, b"DOMAIN-SUFFIX,example.test\n");
        assert_eq!(
            result.checksum,
            u64::from_str_radix(&deterministic_hash(&result.body), 16).unwrap()
        );
        assert!(result.modified_at.is_none());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn performs_live_https_tls_handshake_with_verified_host() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
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
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            let headers = read_headers(&mut stream).await.to_ascii_lowercase();
            assert!(headers.contains("get /rules http/1.1"));
            assert!(headers.contains(&format!("host: localhost:{}", address.port())));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ntls-rule",
                )
                .await
                .unwrap();
        });

        let fetcher =
            ReqwestResourceFetcher::with_test_root_certificate(&[], 1024, &certificate_der)
                .unwrap();
        let mut request = request(&format!("https://localhost:{}/rules", address.port()), 1024);
        // Windows 本地 Rustls 冷启动可能超过 2s；该预算只用于握手测试，不改变生产配置。
        request.deadline = Deadline::new(Instant::now() + Duration::from_secs(10));
        let result = fetcher.fetch(request).await.unwrap();
        let ResourceFetchResult::Modified(result) = result else {
            panic!("expected body")
        };
        assert_eq!(&*result.body, b"tls-rule");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn fetches_through_configured_socks5h_proxy() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();

            let mut connect = [0_u8; 5];
            stream.read_exact(&mut connect).await.unwrap();
            assert_eq!(&connect[..4], &[5, 1, 0, 3]);
            let domain_len = usize::from(connect[4]);
            let mut domain = vec![0_u8; domain_len];
            stream.read_exact(&mut domain).await.unwrap();
            let mut port = [0_u8; 2];
            stream.read_exact(&mut port).await.unwrap();
            assert_eq!(&domain, b"rules.example.test");
            assert_eq!(u16::from_be_bytes(port), 80);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();

            let headers = read_headers(&mut stream).await.to_ascii_lowercase();
            assert!(headers.contains("get /rules http/1.1"));
            assert!(headers.contains("host: rules.example.test"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nproxy-rule",
                )
                .await
                .unwrap();
        });

        let (outbound, root) = proxy_outbound(&format!("socks5h://{address}"));
        let fetcher = ReqwestResourceFetcher::from_resolved(&[outbound], 1024).unwrap();
        let mut resource = request("http://rules.example.test/rules", 1024);
        resource.proxy_profile = Some(ProxyProfileId(Arc::from("proxy")));
        let result = fetcher.fetch(resource).await.unwrap();
        let ResourceFetchResult::Modified(result) = result else {
            panic!("expected body")
        };
        assert_eq!(&*result.body, b"proxy-rule");
        task.await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_non_success_and_oversized_responses() {
        let (address, task) = server(b"blocked", "503 Service Unavailable").await;
        let fetcher = ReqwestResourceFetcher::from_resolved(&[], 1024).unwrap();
        let error = fetcher
            .fetch(request(
                &format!("http://127.0.0.1:{}/rules", address.port()),
                1024,
            ))
            .await
            .unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::ProtocolViolation));
        task.await.unwrap();

        let (address, task) = server(b"too-large", "200 OK").await;
        let error = fetcher
            .fetch(request(
                &format!("http://127.0.0.1:{}/rules", address.port()),
                1,
            ))
            .await
            .unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::ResourceExhausted));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_and_unknown_proxy_profile_fail_before_network() {
        let fetcher = ReqwestResourceFetcher::from_resolved(&[], 1024).unwrap();
        let cancellation = Cancellation::new();
        cancellation.cancel(CancelReason::ClientDisconnected);
        let mut cancelled = request("http://127.0.0.1:9/rules", 1024);
        cancelled.cancellation = cancellation;
        let error = fetcher.fetch(cancelled).await.unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::Cancelled(_)));

        let mut unknown = request("http://127.0.0.1:9/rules", 1024);
        unknown.proxy_profile = Some(ProxyProfileId(Arc::from("missing")));
        let error = fetcher.fetch(unknown).await.unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::InvalidInput));
    }

    #[test]
    fn prepares_proxy_profiles_without_leaking_secret_material() {
        let (outbound, root) = proxy_outbound("socks5://user:password@127.0.0.1:1080");
        let fetcher = ReqwestResourceFetcher::from_resolved(&[outbound], 1024).unwrap();
        let debug = format!("{fetcher:?}");
        assert!(!debug.contains("password"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_invalid_proxy_reference_during_prepare() {
        let outbound = ResolvedOutbound {
            id: ConfigId::new("proxy").unwrap(),
            kind: OutboundType::Socks5,
            proxy_url: ResolvedSecretRef {
                env: None,
                file: Some(std::path::PathBuf::from("/missing/proxy")),
            },
        };
        assert!(matches!(
            ReqwestResourceFetcher::from_resolved(&[outbound], 1024),
            Err(ResourceFetcherBuildError::InvalidOutbound { .. })
        ));
    }
}
