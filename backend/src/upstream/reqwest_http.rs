//! 基于 reqwest 的 HTTP/HTTPS DoH transport adapter。
//!
//! 该 adapter 先提供 direct HTTP/HTTPS 的一次请求边界。共享 `Client` 由
//! reqwest 持有连接池；带显式 `connect_ip` 或 bootstrap 地址的请求暂时按
//! 请求构造解析覆盖 client，连接池 key 与完整 proxy 组合留给后续阶段。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use thiserror::Error;

use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::{
    DOH_MEDIA_TYPE, DohAddressRequest, DohAddressResolver, DohHttpRequest, DohHttpResponseOwned,
    DohHttpTransport, MAX_DOH_RESPONSE_BODY_BYTES,
};

#[derive(Debug, Error)]
pub enum ReqwestDohHttpTransportBuildError {
    #[error("reqwest HTTP client could not be built")]
    Client,
}

#[derive(Clone)]
pub struct ReqwestDohHttpTransport {
    client: reqwest::Client,
    resolver: Arc<dyn DohAddressResolver>,
}

impl std::fmt::Debug for ReqwestDohHttpTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReqwestDohHttpTransport")
            .field("has_resolver", &true)
            .finish()
    }
}

impl ReqwestDohHttpTransport {
    pub fn new(
        resolver: Arc<dyn DohAddressResolver>,
    ) -> Result<Self, ReqwestDohHttpTransportBuildError> {
        let client = client_builder()
            .build()
            .map_err(|_| ReqwestDohHttpTransportBuildError::Client)?;
        Ok(Self { client, resolver })
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
            return Ok(self.client.clone());
        };
        client_builder()
            .resolve_to_addrs(request.host(), &addresses)
            .build()
            .map_err(|_| {
                PortError::new(PortErrorClass::Internal, "reqwest_doh_transport.client")
                    .with_safe_context("reqwest client could not be built")
            })
    }
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
    use std::net::{IpAddr, SocketAddr};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    use super::*;
    use crate::config::resolve::ConfigId;
    use crate::upstream::TokioDohAddressResolver;

    async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
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
}
