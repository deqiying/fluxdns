//! 入站请求与响应关联契约。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::dns::{CancelReason, CanonicalResponse, DnsRequest};

use super::{PortError, PortFuture};

/// Transport 已完成规范化的请求。
#[derive(Debug)]
pub struct InboundRequest {
    request: Arc<DnsRequest>,
    response: ResponseHandle,
}

impl InboundRequest {
    /// 从同一个 request allocation 建立核心输入与 response correlation。
    pub fn new(request: DnsRequest, encoder: Arc<dyn ResponseEncoder>) -> Self {
        let request = Arc::new(request);
        let response = ResponseHandle::new(Arc::clone(&request), encoder);
        Self { request, response }
    }

    pub fn request(&self) -> &DnsRequest {
        &self.request
    }

    pub fn response(&self) -> &ResponseHandle {
        &self.response
    }
}

pub trait InboundAdapter: Send + Sync {
    /// 返回 `None` 表示 adapter 已停止接收新请求。
    ///
    /// `cancellation` 由 Runtime accept loop 持有，用于 shutdown 或 listener drain；
    /// adapter 必须在阻塞 accept/read 期间观察它。
    fn receive<'a>(
        &'a self,
        cancellation: &'a crate::dns::Cancellation,
    ) -> PortFuture<'a, Result<Option<InboundRequest>, PortError>>;
}

/// 把 canonical response 编码并写回原 transport correlation。
pub trait ResponseEncoder: Send + Sync {
    fn encode<'a>(
        &'a self,
        request: &'a DnsRequest,
        response: CanonicalResponse,
    ) -> PortFuture<'a, Result<(), PortError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ResponseState {
    Pending = 0,
    Responded = 1,
    ClientGone = 2,
    Cancelled = 3,
}

impl ResponseState {
    fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Pending,
            1 => Self::Responded,
            2 => Self::ClientGone,
            3 => Self::Cancelled,
            _ => unreachable!("response state is only written by ResponseHandle"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeErrorClass {
    AlreadyResponded,
    ClientGone,
    Cancelled,
    QuestionMismatch,
    EncoderFailure,
}

pub struct EncodeError {
    class: EncodeErrorClass,
    source: Option<PortError>,
}

impl EncodeError {
    fn completed(state: ResponseState) -> Self {
        let class = match state {
            ResponseState::Pending => unreachable!("failed CAS cannot observe Pending"),
            ResponseState::Responded => EncodeErrorClass::AlreadyResponded,
            ResponseState::ClientGone => EncodeErrorClass::ClientGone,
            ResponseState::Cancelled => EncodeErrorClass::Cancelled,
        };
        Self {
            class,
            source: None,
        }
    }

    fn encoder_failure(source: PortError) -> Self {
        Self {
            class: EncodeErrorClass::EncoderFailure,
            source: Some(source),
        }
    }

    fn question_mismatch() -> Self {
        Self {
            class: EncodeErrorClass::QuestionMismatch,
            source: None,
        }
    }

    pub const fn class(&self) -> EncodeErrorClass {
        self.class
    }

    pub const fn source_error(&self) -> Option<&PortError> {
        self.source.as_ref()
    }
}

impl fmt::Debug for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EncodeError")
            .field("class", &self.class)
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "response encode failed: {:?}", self.class)
    }
}

impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

struct ResponseHandleInner {
    state: AtomicU8,
    request: Arc<DnsRequest>,
    encoder: Arc<dyn ResponseEncoder>,
}

/// 原子关联句柄；所有 clone 共享同一状态，因此最多一次进入 encoder。
#[derive(Clone)]
pub struct ResponseHandle {
    inner: Arc<ResponseHandleInner>,
}

impl fmt::Debug for ResponseHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseHandle")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl ResponseHandle {
    fn new(request: Arc<DnsRequest>, encoder: Arc<dyn ResponseEncoder>) -> Self {
        Self {
            inner: Arc::new(ResponseHandleInner {
                state: AtomicU8::new(ResponseState::Pending as u8),
                request,
                encoder,
            }),
        }
    }

    pub fn state(&self) -> ResponseState {
        ResponseState::from_raw(self.inner.state.load(Ordering::Acquire))
    }

    pub async fn respond(&self, response: CanonicalResponse) -> Result<(), EncodeError> {
        if !response.matches_query(&self.inner.request.query) {
            return Err(EncodeError::question_mismatch());
        }

        // 进入 encoder 前先消费唯一响应权。即使 encoder 失败，也保持 Responded，
        // 因为 socket/stream 可能已经发生部分写入，自动重试会破坏 exactly-once。
        self.claim(ResponseState::Responded)?;
        self.inner
            .encoder
            .encode(&self.inner.request, response)
            .await
            .map_err(EncodeError::encoder_failure)
    }

    pub fn mark_client_gone(&self) -> Result<(), EncodeError> {
        // 状态竞争决定是否还能写响应；取消信号无论竞争结果如何都必须传播。
        // 先竞争状态可保证本调用赢得 Pending 时，respond 不会随后抢占写入权。
        let state_result = self.claim(ResponseState::ClientGone);
        self.inner
            .request
            .context
            .meta
            .cancellation
            .cancel(CancelReason::ClientDisconnected);
        state_result
    }

    pub fn cancel(&self, reason: CancelReason) -> Result<(), EncodeError> {
        let state_result = self.claim(ResponseState::Cancelled);
        self.inner.request.context.meta.cancellation.cancel(reason);
        state_result
    }

    fn claim(&self, terminal: ResponseState) -> Result<(), EncodeError> {
        self.inner
            .state
            .compare_exchange(
                ResponseState::Pending as u8,
                terminal as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|observed| EncodeError::completed(ResponseState::from_raw(observed)))
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, ClientIdentity, Deadline,
        DnsMessageId, ListenerId, RequestContext, RequestId, RequestMeta, RuntimeRevision,
        TransportCapabilities as DnsTransportCapabilities, TransportClass,
    };
    use crate::ports::testing::FakeResponseEncoder;
    use crate::ports::{PortErrorClass, PortFuture};

    use super::*;

    struct FailingEncoder;

    impl ResponseEncoder for FailingEncoder {
        fn encode<'a>(
            &'a self,
            _request: &'a DnsRequest,
            _response: CanonicalResponse,
        ) -> PortFuture<'a, Result<(), PortError>> {
            Box::pin(async {
                Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "test_encoder.encode",
                ))
            })
        }
    }

    fn fixture() -> (DnsRequest, CanonicalResponse) {
        let mut query_message = Message::new(42, MessageType::Query, OpCode::Query);
        query_message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        let query = CanonicalQuery::from_message(query_message).unwrap();

        let mut response_message = Message::new(99, MessageType::Response, OpCode::Query);
        response_message.metadata.response_code = ResponseCode::NXDomain;
        response_message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(99))
                .unwrap();

        let now = Instant::now();
        let context = RequestContext {
            meta: RequestMeta {
                request_id: RequestId(1),
                trace_id: None,
                received_at: now,
                received_at_utc: SystemTime::now(),
                deadline: Deadline::new(now + Duration::from_secs(30)),
                cancellation: Cancellation::new(),
                connection_id: None,
                stream_id: None,
                listener_id: ListenerId::from("test"),
                route_id: None,
                original_dns_id: Some(42),
            },
            client: ClientIdentity {
                peer_addr: Some(SocketAddr::from(([127, 0, 0, 1], 5300))),
                ..ClientIdentity::default()
            },
            transport: DnsTransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            runtime_revision: RuntimeRevision(1),
        };
        (DnsRequest { query, context }, response)
    }

    #[test]
    fn inbound_request_uses_one_request_allocation_for_input_and_response() {
        let (request, _) = fixture();
        let inbound = InboundRequest::new(request, Arc::new(FakeResponseEncoder::default()));

        assert!(Arc::ptr_eq(
            &inbound.request,
            &inbound.response.inner.request
        ));
        assert_eq!(
            inbound.request().context.meta.request_id,
            inbound.response.inner.request.context.meta.request_id
        );
    }

    #[tokio::test]
    async fn cloned_handles_encode_exactly_once() {
        let (request, response) = fixture();
        let encoder = Arc::new(FakeResponseEncoder::default());
        let inbound = InboundRequest::new(request, encoder.clone());
        let handle = inbound.response().clone();
        let competing = handle.clone();

        let (first, second) = tokio::join!(
            handle.respond(response.clone()),
            competing.respond(response)
        );

        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let duplicate = first.err().or_else(|| second.err()).unwrap();
        assert_eq!(duplicate.class(), EncodeErrorClass::AlreadyResponded);
        assert_eq!(encoder.encoded_count(), 1);
        assert_eq!(handle.state(), ResponseState::Responded);
    }

    #[tokio::test]
    async fn terminal_non_response_state_rejects_encoding() {
        let (request, response) = fixture();
        let encoder = Arc::new(FakeResponseEncoder::default());
        let inbound = InboundRequest::new(request, encoder.clone());
        let cancellation = inbound.request().context.meta.cancellation.clone();
        let handle = inbound.response().clone();

        handle.mark_client_gone().unwrap();
        let error = handle.respond(response).await.unwrap_err();
        let repeated = handle.mark_client_gone().unwrap_err();
        let cancelled_after_terminal = handle.cancel(CancelReason::Shutdown).unwrap_err();

        assert_eq!(error.class(), EncodeErrorClass::ClientGone);
        assert_eq!(repeated.class(), EncodeErrorClass::ClientGone);
        assert_eq!(
            cancelled_after_terminal.class(),
            EncodeErrorClass::ClientGone
        );
        assert_eq!(
            cancellation.reason(),
            Some(CancelReason::ClientDisconnected)
        );
        assert_eq!(encoder.encoded_count(), 0);
        assert_eq!(handle.state(), ResponseState::ClientGone);
    }

    struct WaitForCancellationEncoder;

    impl ResponseEncoder for WaitForCancellationEncoder {
        fn encode<'a>(
            &'a self,
            request: &'a DnsRequest,
            _response: CanonicalResponse,
        ) -> PortFuture<'a, Result<(), PortError>> {
            Box::pin(async move {
                request.context.meta.cancellation.cancelled().await;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn client_disconnect_reaches_an_in_flight_encoder() {
        let (request, response) = fixture();
        let cancellation = request.context.meta.cancellation.clone();
        let inbound = InboundRequest::new(request, Arc::new(WaitForCancellationEncoder));
        let handle = inbound.response().clone();
        let responding = tokio::spawn({
            let handle = handle.clone();
            async move { handle.respond(response).await }
        });

        tokio::task::yield_now().await;
        assert_eq!(handle.state(), ResponseState::Responded);

        let state_error = handle.mark_client_gone().unwrap_err();
        assert_eq!(state_error.class(), EncodeErrorClass::AlreadyResponded);
        assert_eq!(
            cancellation.reason(),
            Some(CancelReason::ClientDisconnected)
        );
        responding.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn encoder_failure_consumes_response_right_without_retry() {
        let (request, response) = fixture();
        let inbound = InboundRequest::new(request, Arc::new(FailingEncoder));
        let handle = inbound.response().clone();

        let failure = handle.respond(response.clone()).await.unwrap_err();
        let retry = handle.respond(response).await.unwrap_err();

        assert_eq!(failure.class(), EncodeErrorClass::EncoderFailure);
        assert_eq!(retry.class(), EncodeErrorClass::AlreadyResponded);
        assert_eq!(handle.state(), ResponseState::Responded);
    }

    #[tokio::test]
    async fn response_for_another_question_is_rejected_without_consuming_the_handle() {
        let (request, correct_response) = fixture();
        let mut other_query_message = Message::new(7, MessageType::Query, OpCode::Query);
        other_query_message.add_query(Query::query(
            Name::from_str("other.example.").unwrap(),
            RecordType::A,
        ));
        let other_query = CanonicalQuery::from_message(other_query_message).unwrap();
        let mut other_response_message = Message::new(8, MessageType::Response, OpCode::Query);
        other_response_message.add_query(Query::query(
            Name::from_str("other.example.").unwrap(),
            RecordType::A,
        ));
        let other_response = CanonicalResponse::from_message(
            other_response_message,
            &other_query,
            DnsMessageId::new(8),
        )
        .unwrap();

        let encoder = Arc::new(FakeResponseEncoder::default());
        let inbound = InboundRequest::new(request, encoder.clone());
        let handle = inbound.response().clone();

        let mismatch = handle.respond(other_response).await.unwrap_err();
        assert_eq!(mismatch.class(), EncodeErrorClass::QuestionMismatch);
        assert_eq!(handle.state(), ResponseState::Pending);
        assert_eq!(encoder.encoded_count(), 0);

        handle.respond(correct_response).await.unwrap();
        assert_eq!(handle.state(), ResponseState::Responded);
        assert_eq!(encoder.encoded_count(), 1);
    }
}
