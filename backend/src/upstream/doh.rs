//! DoH 上游 HTTP 响应验证。
//!
//! 本模块不建立 HTTP/TLS 连接，只把已读取的 HTTP 响应转换为稳定的
//! UpstreamOutcome，并校验 DNS wire 数据。

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use hickory_proto::op::Message;
use thiserror::Error;
use url::Url;

use crate::dns::{
    CancelReason, Cancellation, CanonicalMessageError, CanonicalQuery, CanonicalResponse, Deadline,
    DnsMessageId, RequestContext,
};
use crate::ports::exchange::{
    ConnectorId, TransportFailure, TransportFailureClass, UpstreamOutcome,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

/// DoH 响应 body 接受的最大 DNS wire 长度。
pub const MAX_DOH_RESPONSE_BODY_BYTES: usize = u16::MAX as usize;
pub const DOH_MEDIA_TYPE: &str = "application/dns-message";

/// 已准备好的 DoH POST 请求；实际 HTTP/TLS 实现由 adapter 提供。
///
/// endpoint 保留 URL host，connect_ip 只作为连接目标提示，因此 adapter
/// 可以在不改变 Host/SNI 的情况下连接到显式地址。
#[derive(Clone)]
pub struct DohHttpRequest {
    endpoint: Url,
    host: Arc<str>,
    sni: Arc<str>,
    connect_ip: Option<IpAddr>,
    body: Arc<[u8]>,
}

impl fmt::Debug for DohHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DohHttpRequest")
            .field("scheme", &self.endpoint.scheme())
            .field("has_host", &true)
            .field("has_sni", &true)
            .field("connect_ip", &self.connect_ip)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl DohHttpRequest {
    fn new(endpoint: Url, connect_ip: Option<IpAddr>, body: Vec<u8>) -> Self {
        let host = Arc::<str>::from(endpoint.host_str().expect("validated DoH endpoint"));
        Self {
            endpoint,
            host: host.clone(),
            sni: host,
            connect_ip,
            body: body.into(),
        }
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn sni(&self) -> &str {
        &self.sni
    }

    pub const fn connect_ip(&self) -> Option<IpAddr> {
        self.connect_ip
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub const fn content_type(&self) -> &'static str {
        DOH_MEDIA_TYPE
    }

    pub const fn accept(&self) -> &'static str {
        DOH_MEDIA_TYPE
    }
}

/// HTTP adapter 返回的拥有所有权的 response envelope。
#[derive(Clone)]
pub struct DohHttpResponseOwned {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl fmt::Debug for DohHttpResponseOwned {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DohHttpResponseOwned")
            .field("status", &self.status)
            .field("has_content_type", &self.content_type.is_some())
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl DohHttpResponseOwned {
    fn as_response(&self) -> DohHttpResponse<'_> {
        DohHttpResponse::new(self.status, self.content_type.as_deref(), &self.body)
    }
}

/// DoH HTTP/TLS adapter 的最小 port；不把具体 HTTP client 类型泄漏到核心层。
pub trait DohHttpTransport: Send + Sync {
    fn post<'a>(
        &'a self,
        request: DohHttpRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>>;
}

/// 已绑定 endpoint/profile 的 DoH exchange。
///
/// 网络解析和连接由 DohHttpTransport 完成；本 connector 固定 POST 语义，
/// 负责请求 ID 关联、connect_ip 传递、取消和 response protocol validation。
pub struct DohExchange<T> {
    connector: ConnectorId,
    endpoint: Url,
    connect_ip: Option<IpAddr>,
    transport: Arc<T>,
    next_id: AtomicU16,
}

impl<T> fmt::Debug for DohExchange<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DohExchange")
            .field("connector", &self.connector)
            .field("scheme", &self.endpoint.scheme())
            .field("has_connect_ip", &self.connect_ip.is_some())
            .finish()
    }
}

impl<T: DohHttpTransport> DohExchange<T> {
    pub fn new(
        connector: ConnectorId,
        endpoint: Url,
        connect_ip: Option<IpAddr>,
        transport: Arc<T>,
    ) -> Result<Self, DohEndpointError> {
        validate_endpoint(&endpoint)?;
        Ok(Self {
            connector,
            endpoint,
            connect_ip,
            transport,
            next_id: AtomicU16::new(1),
        })
    }

    pub fn connector_id(&self) -> &ConnectorId {
        &self.connector
    }

    fn allocate_id(&self) -> DnsMessageId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        DnsMessageId::new(if id == 0 { 1 } else { id })
    }
}

impl<T: DohHttpTransport + 'static> crate::ports::exchange::DnsExchange for DohExchange<T> {
    fn connector_id(&self) -> &ConnectorId {
        &self.connector
    }

    fn exchange<'a>(
        &'a self,
        query: &'a CanonicalQuery,
        context: &'a RequestContext,
    ) -> PortFuture<'a, UpstreamOutcome> {
        Box::pin(async move {
            if context.meta.cancellation.is_cancelled() {
                return UpstreamOutcome::Cancelled(
                    context
                        .meta
                        .cancellation
                        .reason()
                        .unwrap_or(CancelReason::UpstreamCancelled),
                );
            }

            let id = self.allocate_id();
            let body = match query.message_with_id(id).to_vec() {
                Ok(body) if body.len() <= MAX_DOH_RESPONSE_BODY_BYTES => body,
                Ok(_) => {
                    return transport_failure(
                        &self.connector,
                        TransportFailureClass::BodyLimit,
                        false,
                        Some("DoH request body exceeded the DNS wire limit"),
                    );
                }
                Err(_) => {
                    return transport_failure(
                        &self.connector,
                        TransportFailureClass::ProtocolViolation,
                        false,
                        Some("DoH request DNS wire could not be encoded"),
                    );
                }
            };

            let request = DohHttpRequest::new(self.endpoint.clone(), self.connect_ip, body);
            match self
                .transport
                .post(request, context.meta.deadline, &context.meta.cancellation)
                .await
            {
                Ok(response) => validate_response(
                    &self.connector,
                    query,
                    id,
                    response.as_response(),
                    &context.meta.cancellation,
                ),
                Err(error) => map_port_error(&self.connector, error),
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum DohEndpointError {
    #[error("DoH endpoint must use http or https")]
    UnsupportedScheme,
    #[error("DoH endpoint must include a host")]
    MissingHost,
    #[error("DoH endpoint must not contain credentials")]
    Credentials,
    #[error("DoH endpoint must not contain a fragment")]
    Fragment,
}

fn validate_endpoint(endpoint: &Url) -> Result<(), DohEndpointError> {
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err(DohEndpointError::UnsupportedScheme);
    }
    if endpoint.host_str().is_none() {
        return Err(DohEndpointError::MissingHost);
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(DohEndpointError::Credentials);
    }
    if endpoint.fragment().is_some() {
        return Err(DohEndpointError::Fragment);
    }
    Ok(())
}

fn map_port_error(connector: &ConnectorId, error: PortError) -> UpstreamOutcome {
    let class = error.class();
    if let PortErrorClass::Cancelled(reason) = class {
        return UpstreamOutcome::Cancelled(*reason);
    }
    let (failure_class, retryable, context) = match class {
        PortErrorClass::Timeout => (
            TransportFailureClass::Timeout,
            true,
            Some("DoH HTTP request timed out"),
        ),
        PortErrorClass::Unavailable => (
            TransportFailureClass::Connect,
            true,
            Some("DoH HTTP transport was unavailable"),
        ),
        PortErrorClass::ResourceExhausted => (
            TransportFailureClass::ResourceExhausted,
            true,
            Some("DoH HTTP transport was resource limited"),
        ),
        PortErrorClass::ProtocolViolation | PortErrorClass::CorruptData => (
            TransportFailureClass::ProtocolViolation,
            false,
            Some("DoH HTTP transport returned invalid data"),
        ),
        PortErrorClass::InvalidInput
        | PortErrorClass::PermissionDenied
        | PortErrorClass::Internal
        | PortErrorClass::Cancelled(_) => (
            TransportFailureClass::Internal,
            false,
            Some("DoH HTTP transport failed"),
        ),
    };
    transport_failure(connector, failure_class, retryable, context)
}

fn transport_failure(
    connector: &ConnectorId,
    class: TransportFailureClass,
    retryable: bool,
    safe_context: Option<&'static str>,
) -> UpstreamOutcome {
    UpstreamOutcome::TransportFailure(TransportFailure {
        connector: connector.clone(),
        class,
        retryable,
        safe_context,
    })
}

/// 已读取的 HTTP response envelope。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DohHttpResponse<'a> {
    pub status: u16,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
}

impl<'a> DohHttpResponse<'a> {
    pub const fn new(status: u16, content_type: Option<&'a str>, body: &'a [u8]) -> Self {
        Self {
            status,
            content_type,
            body,
        }
    }
}

/// 校验一个 DoH HTTP response 并生成 upstream outcome。
///
/// 校验顺序为取消、HTTP 状态、媒体类型、body 大小，以及 DNS wire/request 关联。
/// 拒绝结果只暴露稳定的 TransportFailureClass，不包含 URL、body 或原始解析文本。
pub fn validate_response(
    connector: &ConnectorId,
    query: &CanonicalQuery,
    expected_id: DnsMessageId,
    response: DohHttpResponse<'_>,
    cancellation: &Cancellation,
) -> UpstreamOutcome {
    if cancellation.is_cancelled() {
        return UpstreamOutcome::Cancelled(
            cancellation
                .reason()
                .unwrap_or(CancelReason::UpstreamCancelled),
        );
    }

    if !(200..300).contains(&response.status) {
        return failure(
            connector,
            TransportFailureClass::HttpStatus,
            is_retryable_status(response.status),
            Some("DoH response status was not successful"),
        );
    }

    if !is_dns_message_content_type(response.content_type) {
        return failure(
            connector,
            TransportFailureClass::MediaType,
            false,
            Some("DoH response media type was not application/dns-message"),
        );
    }

    if response.body.len() > MAX_DOH_RESPONSE_BODY_BYTES {
        return failure(
            connector,
            TransportFailureClass::BodyLimit,
            false,
            Some("DoH response body exceeded the DNS wire limit"),
        );
    }

    let message = match Message::from_vec(response.body) {
        Ok(message) => message,
        Err(_) => {
            return failure(
                connector,
                TransportFailureClass::Wire,
                false,
                Some("DoH response DNS wire was invalid"),
            );
        }
    };

    match CanonicalResponse::from_message(message, query, expected_id) {
        Ok(response) => UpstreamOutcome::Response(response),
        Err(error) => failure(
            connector,
            classify_canonical_error(&error),
            false,
            Some(canonical_error_context(&error)),
        ),
    }
}

fn is_dns_message_content_type(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(DOH_MEDIA_TYPE))
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || (500..600).contains(&status)
}

fn classify_canonical_error(error: &CanonicalMessageError) -> TransportFailureClass {
    match error {
        CanonicalMessageError::MessageIdMismatch { .. } => TransportFailureClass::ProtocolViolation,
        CanonicalMessageError::QuestionMismatch => TransportFailureClass::QuestionMismatch,
        CanonicalMessageError::UnexpectedMessageType { .. }
        | CanonicalMessageError::UnsupportedOpCode(_)
        | CanonicalMessageError::QuestionCount(_)
        | CanonicalMessageError::UnsupportedEdnsVersion(_) => {
            TransportFailureClass::ProtocolViolation
        }
    }
}

fn canonical_error_context(error: &CanonicalMessageError) -> &'static str {
    match error {
        CanonicalMessageError::MessageIdMismatch { .. } => "DoH response ID did not match request",
        CanonicalMessageError::QuestionMismatch => "DoH response question did not match request",
        CanonicalMessageError::UnexpectedMessageType { .. } => {
            "DoH response did not have the DNS response flag"
        }
        CanonicalMessageError::UnsupportedOpCode(_) => "DoH response used an unsupported opcode",
        CanonicalMessageError::QuestionCount(_) => "DoH response had an invalid question count",
        CanonicalMessageError::UnsupportedEdnsVersion(_) => {
            "DoH response used an unsupported EDNS version"
        }
    }
}

fn failure(
    connector: &ConnectorId,
    class: TransportFailureClass,
    retryable: bool,
    safe_context: Option<&'static str>,
) -> UpstreamOutcome {
    UpstreamOutcome::TransportFailure(TransportFailure {
        connector: connector.clone(),
        class,
        retryable,
        safe_context,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_proto::{
        op::{Message, MessageType, OpCode, Query, ResponseCode},
        rr::{Name, RecordType},
    };

    use crate::dns::{CancelReason, CanonicalQuery, DnsMessageId};
    use crate::ports::exchange::{TransportFailureClass, UpstreamOutcome};

    use super::{DohHttpResponse, MAX_DOH_RESPONSE_BODY_BYTES, validate_response};

    fn connector() -> crate::ports::exchange::ConnectorId {
        crate::ports::exchange::ConnectorId::new("resolver-a").unwrap()
    }

    fn query() -> CanonicalQuery {
        let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("Example.COM.").unwrap(),
            RecordType::A,
        ));
        CanonicalQuery::from_message(message).unwrap()
    }

    fn wire_response(query: &CanonicalQuery, id: u16, code: ResponseCode) -> Vec<u8> {
        let mut message = Message::new(id, MessageType::Response, OpCode::Query);
        message.metadata.response_code = code;
        message.add_query(Query::query(
            query.question().name().clone(),
            query.question().query_type(),
        ));
        message.to_vec().unwrap()
    }

    fn outcome(status: u16, content_type: Option<&str>, body: &[u8]) -> UpstreamOutcome {
        validate_response(
            &connector(),
            &query(),
            DnsMessageId::new(0x1234),
            DohHttpResponse::new(status, content_type, body),
            &crate::dns::Cancellation::new(),
        )
    }

    #[test]
    fn accepts_successful_dns_message_with_content_type_parameters() {
        let query = query();
        let body = wire_response(&query, 0x1234, ResponseCode::NoError);
        let result = validate_response(
            &connector(),
            &query,
            DnsMessageId::new(0x1234),
            DohHttpResponse::new(200, Some(" Application/DNS-Message; charset=binary"), &body),
            &crate::dns::Cancellation::new(),
        );

        assert!(matches!(result, UpstreamOutcome::Response(_)));
    }

    #[test]
    fn rejects_status_media_type_and_body_boundaries() {
        assert!(matches!(
            outcome(503, Some("application/dns-message"), &[]),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::HttpStatus && failure.retryable
        ));
        assert!(matches!(
            outcome(200, Some("application/json"), &[]),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::MediaType
        ));

        let body = vec![0_u8; MAX_DOH_RESPONSE_BODY_BYTES + 1];
        assert!(matches!(
            outcome(200, Some("application/dns-message"), &body),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::BodyLimit
        ));
    }

    #[test]
    fn rejects_invalid_wire_id_and_question_mismatch() {
        assert!(matches!(
            outcome(200, Some("application/dns-message"), &[0xff, 0x00]),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::Wire
        ));

        let expected_query = query();
        let mut query_wire = Message::new(0x1234, MessageType::Query, OpCode::Query);
        query_wire.add_query(Query::query(
            expected_query.question().name().clone(),
            expected_query.question().query_type(),
        ));
        let query_wire = query_wire.to_vec().unwrap();
        assert!(matches!(
            validate_response(
                &connector(),
                &expected_query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(200, Some("application/dns-message"), &query_wire),
                &crate::dns::Cancellation::new(),
            ),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::ProtocolViolation
        ));

        let body = wire_response(&expected_query, 0x4321, ResponseCode::NoError);
        assert!(matches!(
            validate_response(
                &connector(),
                &expected_query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(200, Some("application/dns-message"), &body),
                &crate::dns::Cancellation::new(),
            ),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::ProtocolViolation
        ));

        let other_query = {
            let mut message = Message::new(1, MessageType::Query, OpCode::Query);
            message.add_query(Query::query(
                Name::from_str("other.example.").unwrap(),
                RecordType::A,
            ));
            CanonicalQuery::from_message(message).unwrap()
        };
        let body = wire_response(&other_query, 0x1234, ResponseCode::NoError);
        assert!(matches!(
            validate_response(
                &connector(),
                &expected_query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(200, Some("application/dns-message"), &body),
                &crate::dns::Cancellation::new(),
            ),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::QuestionMismatch
        ));
    }

    #[test]
    fn preserves_terminal_dns_response_and_prioritizes_cancellation() {
        let query = query();
        let body = wire_response(&query, 0x1234, ResponseCode::NXDomain);
        assert!(matches!(
            validate_response(
                &connector(),
                &query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(200, Some("application/dns-message"), &body),
                &crate::dns::Cancellation::new(),
            ),
            UpstreamOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NxDomain
        ));

        let cancellation = crate::dns::Cancellation::new();
        cancellation.cancel(CancelReason::DeadlineExceeded);
        assert!(matches!(
            validate_response(
                &connector(),
                &query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(500, Some("application/json"), &[0xff]),
                &cancellation,
            ),
            UpstreamOutcome::Cancelled(CancelReason::DeadlineExceeded)
        ));
    }
}

#[cfg(test)]
mod connector_tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};
    use url::Url;

    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, Deadline, ListenerId, RequestContext,
        RequestId, RequestMeta, RuntimeRevision, TransportCapabilities, TransportClass,
    };
    use crate::ports::exchange::{DnsExchange, TransportFailureClass, UpstreamOutcome};
    use crate::ports::{PortError, PortErrorClass, PortFuture};

    use super::{
        DohEndpointError, DohExchange, DohHttpRequest, DohHttpResponseOwned, DohHttpTransport,
    };

    struct FakeTransport {
        request: Mutex<Option<DohHttpRequest>>,
        result: Mutex<Option<Result<DohHttpResponseOwned, PortError>>>,
    }

    impl FakeTransport {
        fn success(body: Vec<u8>) -> Self {
            Self {
                request: Mutex::new(None),
                result: Mutex::new(Some(Ok(DohHttpResponseOwned {
                    status: 200,
                    content_type: Some("application/dns-message".to_owned()),
                    body,
                }))),
            }
        }

        fn timeout() -> Self {
            Self {
                request: Mutex::new(None),
                result: Mutex::new(Some(Err(PortError::new(
                    PortErrorClass::Timeout,
                    "fake_doh.timeout",
                )))),
            }
        }
    }

    impl DohHttpTransport for FakeTransport {
        fn post<'a>(
            &'a self,
            request: DohHttpRequest,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
            *self.request.lock().unwrap() = Some(request);
            let result = self.result.lock().unwrap().take().unwrap();
            Box::pin(async move { result })
        }
    }

    fn query() -> CanonicalQuery {
        let mut message = Message::new(0x4321, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("Example.COM.").unwrap(),
            RecordType::A,
        ));
        CanonicalQuery::from_message(message).unwrap()
    }

    fn response_wire(query: &CanonicalQuery, id: u16) -> Vec<u8> {
        let mut message = Message::new(id, MessageType::Response, OpCode::Query);
        message.metadata.response_code = ResponseCode::NoError;
        message.add_query(Query::query(
            query.question().name().clone(),
            query.question().query_type(),
        ));
        message.to_vec().unwrap()
    }

    fn context() -> RequestContext {
        RequestContext {
            meta: RequestMeta {
                request_id: RequestId(1),
                trace_id: None,
                received_at: Instant::now(),
                received_at_utc: SystemTime::now(),
                deadline: Deadline::new(Instant::now() + Duration::from_secs(5)),
                cancellation: Cancellation::new(),
                connection_id: None,
                stream_id: None,
                listener_id: ListenerId::from("test"),
                route_id: None,
                original_dns_id: Some(0x4321),
            },
            client: Default::default(),
            transport: TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            runtime_revision: RuntimeRevision(1),
        }
    }

    #[tokio::test]
    async fn exchange_preserves_host_sni_and_connect_ip_and_validates_response() {
        let query = query();
        let transport = Arc::new(FakeTransport::success(response_wire(&query, 1)));
        let exchange = DohExchange::new(
            crate::ports::exchange::ConnectorId::new("remote").unwrap(),
            Url::parse("https://dns.example.test/dns-query").unwrap(),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 44))),
            transport.clone(),
        )
        .unwrap();

        assert!(matches!(
            exchange.exchange(&query, &context()).await,
            UpstreamOutcome::Response(_)
        ));
        let request = transport.request.lock().unwrap().clone().unwrap();
        assert_eq!(
            request.endpoint().as_str(),
            "https://dns.example.test/dns-query"
        );
        assert_eq!(request.host(), "dns.example.test");
        assert_eq!(request.sni(), "dns.example.test");
        assert_eq!(
            request.connect_ip(),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 44)))
        );
        assert_eq!(Message::from_vec(request.body()).unwrap().id, 1);
    }

    #[tokio::test]
    async fn transport_timeout_maps_to_retryable_upstream_failure() {
        let transport = Arc::new(FakeTransport::timeout());
        let exchange = DohExchange::new(
            crate::ports::exchange::ConnectorId::new("remote").unwrap(),
            Url::parse("http://dns.example.test/dns-query").unwrap(),
            None,
            transport,
        )
        .unwrap();

        assert!(matches!(
            exchange.exchange(&query(), &context()).await,
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::Timeout && failure.retryable
        ));
    }

    #[test]
    fn endpoint_rejects_unsafe_url_shapes() {
        let transport = Arc::new(FakeTransport::timeout());
        let connector = crate::ports::exchange::ConnectorId::new("remote").unwrap();
        assert!(matches!(
            DohExchange::new(
                connector.clone(),
                Url::parse("ftp://dns.example.test/query").unwrap(),
                None,
                transport.clone()
            ),
            Err(DohEndpointError::UnsupportedScheme)
        ));
        assert!(matches!(
            DohExchange::new(
                connector.clone(),
                Url::parse("https://user:secret@dns.example.test/query").unwrap(),
                None,
                transport.clone()
            ),
            Err(DohEndpointError::Credentials)
        ));
        assert!(matches!(
            DohExchange::new(
                connector,
                Url::parse("https://dns.example.test/query#fragment").unwrap(),
                None,
                transport
            ),
            Err(DohEndpointError::Fragment)
        ));
    }
}
