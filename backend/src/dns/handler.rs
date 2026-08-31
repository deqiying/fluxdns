//! Transport 无关的最小 DNS Core handler。

use std::time::Instant;

use hickory_proto::op::ResponseCode;
use thiserror::Error;

use crate::ports::PortFuture;

use super::{CanonicalMessageError, CanonicalResponse, DnsRequest};

/// Core 对一个入站请求的终态结果。
#[derive(Debug, Eq, PartialEq)]
pub enum CoreOutcome {
    Response(CanonicalResponse),
    NoResponse,
}

/// DNS Core 构造响应时的稳定错误分类。
#[derive(Debug, Error, Eq, PartialEq)]
pub enum CoreError {
    #[error("canonical response could not be constructed")]
    ResponseConstruction(#[source] CanonicalMessageError),
}

/// Transport 无关的 DNS 请求处理器。
pub trait DnsCore: Send + Sync {
    fn resolve<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, Result<CoreOutcome, CoreError>>;
}

/// 在真实策略、hosts 和 upstream 接线前使用的确定性安全默认 handler。
#[derive(Clone, Copy, Debug, Default)]
pub struct ServFailCore;

impl DnsCore for ServFailCore {
    fn resolve<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, Result<CoreOutcome, CoreError>> {
        Box::pin(async move {
            let meta = &request.context.meta;
            if meta.cancellation.is_cancelled() || meta.deadline.is_expired(Instant::now()) {
                return Ok(CoreOutcome::NoResponse);
            }

            CanonicalResponse::empty_response(&request.query, ResponseCode::ServFail)
                .map(CoreOutcome::Response)
                .map_err(CoreError::ResponseConstruction)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use crate::dns::{
        CacheCompatibilityKey, CancelReason, Cancellation, CanonicalQuery, Deadline, DnsMessageId,
        DnsRequest, ListenerId, RequestContext, RequestId, RequestMeta, RuntimeRevision,
        TransportCapabilities, TransportClass,
    };

    use super::{CoreOutcome, DnsCore, ServFailCore};

    fn request(id: u16) -> DnsRequest {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("Example.COM").unwrap(),
            RecordType::A,
        ));
        let query = CanonicalQuery::from_message(message).unwrap();
        let now = Instant::now();
        DnsRequest {
            query,
            context: RequestContext {
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
                    original_dns_id: Some(id),
                },
                client: crate::dns::ClientIdentity {
                    peer_addr: Some(SocketAddr::from(([127, 0, 0, 1], 5300))),
                    ..crate::dns::ClientIdentity::default()
                },
                transport: TransportCapabilities {
                    class: TransportClass::Datagram,
                    cache_compatibility: CacheCompatibilityKey(1),
                },
                runtime_revision: RuntimeRevision(1),
            },
        }
    }

    #[tokio::test]
    async fn returns_canonical_servfail_without_transport_id() {
        let first = request(7);
        let second = request(0xbeef);
        let core = ServFailCore;

        let first = core.resolve(&first).await.unwrap();
        let second = core.resolve(&second).await.unwrap();

        let CoreOutcome::Response(first) = first else {
            panic!("expected response");
        };
        let CoreOutcome::Response(second) = second else {
            panic!("expected response");
        };
        assert_eq!(first, second);
        assert_eq!(first.as_message().metadata.id, DnsMessageId::new(0).value());
        assert_eq!(first.class(), crate::dns::ResponseClass::ServFail);
    }

    #[tokio::test]
    async fn cancelled_request_returns_no_response() {
        let request = request(7);
        request
            .context
            .meta
            .cancellation
            .cancel(CancelReason::ClientDisconnected);

        assert_eq!(
            ServFailCore.resolve(&request).await.unwrap(),
            CoreOutcome::NoResponse
        );
    }

    #[tokio::test]
    async fn expired_request_returns_no_response() {
        let mut request = request(7);
        let now = Instant::now();
        request.context.meta.deadline = Deadline::new(now - Duration::from_secs(1));

        assert_eq!(
            ServFailCore.resolve(&request).await.unwrap(),
            CoreOutcome::NoResponse
        );
    }
}
