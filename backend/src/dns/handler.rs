//! Transport 无关的最小 DNS Core handler。

use std::time::Instant;

use hickory_proto::op::ResponseCode;
use hickory_proto::rr::{RData, Record, RecordType, rdata::A, rdata::AAAA};
use thiserror::Error;

use crate::ports::PortFuture;
use crate::ports::inbound::{EncodeErrorClass, InboundRequest};

use super::{CanonicalMessageError, CanonicalResponse, DnsRequest, HostsTable};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Responded,
    NoResponse,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DispatchError {
    #[error("DNS Core failed: {0}")]
    Core(#[from] CoreError),
    #[error("response encoder failed: {class:?}")]
    Encode { class: EncodeErrorClass },
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

/// 基于不可变 hosts snapshot 的最小本地解析器。
#[derive(Clone, Debug)]
pub struct HostsCore {
    table: std::sync::Arc<HostsTable>,
    ttl: u32,
}

impl HostsCore {
    pub fn new(table: HostsTable, ttl: u32) -> Self {
        Self {
            table: std::sync::Arc::new(table),
            ttl,
        }
    }

    pub fn table(&self) -> &HostsTable {
        &self.table
    }
}

impl DnsCore for HostsCore {
    fn resolve<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, Result<CoreOutcome, CoreError>> {
        Box::pin(async move {
            let meta = &request.context.meta;
            if meta.cancellation.is_cancelled() || meta.deadline.is_expired(Instant::now()) {
                return Ok(CoreOutcome::NoResponse);
            }

            let question = request.query.question();
            let answers = self
                .table
                .lookup(question.name(), question.query_type())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|address| match (question.query_type(), address) {
                    (RecordType::A, std::net::IpAddr::V4(address)) => Some(Record::from_rdata(
                        question.name().clone(),
                        self.ttl,
                        RData::A(A(address)),
                    )),
                    (RecordType::AAAA, std::net::IpAddr::V6(address)) => Some(Record::from_rdata(
                        question.name().clone(),
                        self.ttl,
                        RData::AAAA(AAAA(address)),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let code = if answers.is_empty() && !self.table.contains_name(question.name()) {
                ResponseCode::NXDomain
            } else {
                ResponseCode::NoError
            };
            let response = if code == ResponseCode::NoError && !answers.is_empty() {
                CanonicalResponse::response_with_answers(&request.query, answers)
            } else {
                CanonicalResponse::response_with_code(&request.query, code, answers)
            };
            response
                .map(CoreOutcome::Response)
                .map_err(CoreError::ResponseConstruction)
        })
    }
}

/// 将一条已规范化的入站请求交给 Core，并通过唯一 response handle 完成响应。
pub async fn dispatch_inbound(
    core: &dyn DnsCore,
    inbound: InboundRequest,
) -> Result<DispatchOutcome, DispatchError> {
    let outcome = core.resolve(inbound.request()).await?;
    match outcome {
        CoreOutcome::Response(response) => inbound
            .response()
            .respond(response)
            .await
            .map(|_| DispatchOutcome::Responded)
            .map_err(|error| DispatchError::Encode {
                class: error.class(),
            }),
        CoreOutcome::NoResponse => Ok(DispatchOutcome::NoResponse),
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
        DnsRequest, HostsTable, ListenerId, RequestContext, RequestId, RequestMeta,
        RuntimeRevision, TransportCapabilities, TransportClass,
    };

    use crate::ports::inbound::InboundRequest;
    use crate::ports::testing::FakeResponseEncoder;

    use super::{CoreOutcome, DispatchOutcome, DnsCore, HostsCore, ServFailCore, dispatch_inbound};

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

    #[tokio::test]
    async fn dispatches_core_response_through_exactly_once_encoder() {
        let request = request(7);
        let encoder = std::sync::Arc::new(FakeResponseEncoder::default());
        let inbound = InboundRequest::new(request, encoder.clone());

        assert_eq!(
            dispatch_inbound(&ServFailCore, inbound).await.unwrap(),
            DispatchOutcome::Responded
        );
        assert_eq!(encoder.encoded_count(), 1);
    }

    #[tokio::test]
    async fn dispatch_preserves_no_response_when_request_is_cancelled() {
        let request = request(7);
        request
            .context
            .meta
            .cancellation
            .cancel(CancelReason::ClientDisconnected);
        let encoder = std::sync::Arc::new(FakeResponseEncoder::default());
        let inbound = InboundRequest::new(request, encoder.clone());

        assert_eq!(
            dispatch_inbound(&ServFailCore, inbound).await.unwrap(),
            DispatchOutcome::NoResponse
        );
        assert_eq!(encoder.encoded_count(), 0);
    }

    #[tokio::test]
    async fn hosts_core_returns_local_answers_and_nxdomain() {
        let table = HostsTable::parse("192.0.2.1 example.com\n").unwrap();
        let core = HostsCore::new(table, 60);

        let answer = core.resolve(&request(7)).await.unwrap();
        let CoreOutcome::Response(answer) = answer else {
            panic!("expected local answer");
        };
        assert_eq!(answer.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(answer.ttl().min_ttl, Some(60));
        assert_eq!(answer.as_message().answers.len(), 1);

        let mut unknown = request(8);
        unknown.query = crate::dns::CanonicalQuery::from_message({
            let mut message = hickory_proto::op::Message::new(
                8,
                hickory_proto::op::MessageType::Query,
                hickory_proto::op::OpCode::Query,
            );
            message.add_query(hickory_proto::op::Query::query(
                hickory_proto::rr::Name::from_ascii("missing.example.").unwrap(),
                hickory_proto::rr::RecordType::A,
            ));
            message
        })
        .unwrap();
        let unknown = core.resolve(&unknown).await.unwrap();
        let CoreOutcome::Response(unknown) = unknown else {
            panic!("expected nxdomain");
        };
        assert_eq!(unknown.class(), crate::dns::ResponseClass::NxDomain);
    }

    #[tokio::test]
    async fn hosts_core_returns_nodata_for_known_name_without_requested_family() {
        let table = HostsTable::parse("192.0.2.1 example.com\n").unwrap();
        let core = HostsCore::new(table, 60);
        let mut request = request(7);
        request.query = crate::dns::CanonicalQuery::from_message({
            let mut message = hickory_proto::op::Message::new(
                7,
                hickory_proto::op::MessageType::Query,
                hickory_proto::op::OpCode::Query,
            );
            message.add_query(hickory_proto::op::Query::query(
                hickory_proto::rr::Name::from_ascii("example.com.").unwrap(),
                hickory_proto::rr::RecordType::AAAA,
            ));
            message
        })
        .unwrap();

        let response = core.resolve(&request).await.unwrap();
        let CoreOutcome::Response(response) = response else {
            panic!("expected nodata");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::NoData);
    }
}
