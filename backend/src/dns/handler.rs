//! Transport 无关的最小 DNS Core handler。

use std::sync::Arc;
use std::time::Instant;

use hickory_proto::op::ResponseCode;
use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A, rdata::AAAA, rdata::CNAME};
use thiserror::Error;

use crate::cache::CacheCommitCandidate;
use crate::ports::PortFuture;
use crate::ports::inbound::{EncodeErrorClass, InboundRequest};
use crate::ports::storage::StatsSource;
use crate::ports::telemetry::CacheStatus;
use crate::resource::{CanonicalDomain, HostsIndex, HostsLookup, HostsRecord, ResourceVersion};

use super::{CanonicalMessageError, CanonicalResponse, DnsRequest, HostsTable};

/// Core 对一个入站请求的终态结果。
#[derive(Debug, Eq, PartialEq)]
pub enum CoreOutcome {
    Response(Arc<CanonicalResponse>),
    NoResponse,
}

/// Core 返回给进程级完成事件 publisher 的一次性结果。
#[derive(Debug)]
pub struct DnsCoreCompletion {
    pub result: Result<CoreOutcome, CoreError>,
    pub observation: Option<DnsResolutionObservation>,
    /// 由 Core 内部取消路径给出的终态原因；可能不同于入站 request token 的状态。
    pub cancellation_reason: Option<crate::dns::CancelReason>,
    pub cache_commit: Option<CacheCommitCandidate>,
}

impl DnsCoreCompletion {
    pub fn from_result(result: Result<CoreOutcome, CoreError>) -> Self {
        Self {
            result,
            observation: None,
            cancellation_reason: None,
            cache_commit: None,
        }
    }
}

/// Core 在完成请求后可选提供的低基数解析元数据。
///
/// 默认 Core 不需要实现该观察接口；策略 Core 会提供实际命中的 strategy、
/// selected upstream、answer source 和 cache 状态，供详情日志与聚合统计复用同一份判定结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DnsResolutionObservation {
    pub client_bucket: Option<Arc<str>>,
    pub strategy_id: Option<Arc<str>>,
    /// 当前请求实际命中的 listener/strategy rule 摘要。
    pub matched_rule: Option<MatchedRuleObservation>,
    /// 策略选中的 direct upstream 或 group ID。该值只允许来自已校验配置。
    pub upstream_id: Option<Arc<str>>,
    /// group 实际选中的顶层成员 ID；cache hit 时保留缓存生产请求的 member。
    pub upstream_member_id: Option<Arc<str>>,
    /// 产生响应内容的 direct/member ID；cache hit 时表示缓存生产来源。
    pub upstream_used_id: Option<Arc<str>>,
    pub source: StatsSource,
    pub cache_status: CacheStatus,
}

/// 一条已命中规则的低基数来源，不携带规则文本或 matcher 内容。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchedRuleSource {
    ListenerHosts,
    StrategyHosts,
    RuleSet,
}

/// Policy 已判定的规则命中摘要；资源 ID 只来自已验证配置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchedRuleObservation {
    /// 命中来自 listener hosts、strategy hosts 或 rule-set。
    pub source: MatchedRuleSource,
    /// 被命中的 hosts/rule-set 配置资源 ID。
    pub resource_id: Arc<str>,
    /// 生成当前匹配结果的资源 epoch/revision；缺失时不推测版本。
    pub resource_version: Option<ResourceVersion>,
    /// listener hosts 没有 strategy rule 序号，因此为 `None`。
    pub ordinal: Option<u64>,
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

    /// 返回请求终态及可选低基数观察数据。
    ///
    /// 该默认实现保持既有 Core 的行为和对象安全性；需要观测的实现只需覆盖
    /// 此方法，调用方不会再从请求字段推测 cache/source/strategy。
    fn resolve_with_completion<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, DnsCoreCompletion> {
        Box::pin(async move { DnsCoreCompletion::from_result(self.resolve(request).await) })
    }

    /// 兼容只关注策略观察值的单元测试；生产路径只使用一次性完成事件。
    #[cfg(test)]
    fn resolve_with_observation<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<
        'a,
        (
            Result<CoreOutcome, CoreError>,
            Option<DnsResolutionObservation>,
        ),
    > {
        Box::pin(async move {
            let completion = self.resolve_with_completion(request).await;
            if let Some(candidate) = completion.cache_commit {
                let _ = candidate
                    .commit(std::time::Duration::from_millis(100))
                    .await;
            }
            (completion.result, completion.observation)
        })
    }
}

/// 用于 dispatch 与 Transport 契约测试的确定性 SERVFAIL handler。
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
                .map(Arc::new)
                .map(CoreOutcome::Response)
                .map_err(CoreError::ResponseConstruction)
        })
    }
}

/// 基于不可变 hosts snapshot 的最小本地解析器。
#[derive(Clone, Debug)]
pub struct HostsCore {
    table: Arc<HostsTable>,
    resource_indexes: Option<Arc<Vec<HostsIndex>>>,
    ttl: u32,
}

impl HostsCore {
    pub fn new(table: HostsTable, ttl: u32) -> Self {
        Self {
            table: Arc::new(table),
            resource_indexes: None,
            ttl,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_resource_indexes(indexes: Vec<HostsIndex>, ttl: u32) -> Self {
        Self {
            table: Arc::new(HostsTable::parse("").expect("empty hosts table is valid")),
            resource_indexes: Some(Arc::new(indexes)),
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
            let (answers, known_name) = if let Some(indexes) = &self.resource_indexes {
                resource_answers(indexes, question.name(), question.query_type(), self.ttl)
            } else {
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
                        (RecordType::AAAA, std::net::IpAddr::V6(address)) => {
                            Some(Record::from_rdata(
                                question.name().clone(),
                                self.ttl,
                                RData::AAAA(AAAA(address)),
                            ))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                (answers, self.table.contains_name(question.name()))
            };
            let code = if answers.is_empty() && !known_name {
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
                .map(Arc::new)
                .map(CoreOutcome::Response)
                .map_err(CoreError::ResponseConstruction)
        })
    }
}

pub(super) fn resource_answers(
    indexes: &[HostsIndex],
    name: &Name,
    query_type: RecordType,
    ttl: u32,
) -> (Vec<Record>, bool) {
    let Ok(domain) = CanonicalDomain::parse(&name.to_ascii()) else {
        return (Vec::new(), false);
    };
    let has_exact = indexes.iter().any(|index| index.records(&domain).is_some());
    let mut known_name = false;
    let mut answers = Vec::new();

    for index in indexes {
        let lookup = if has_exact {
            index.records(&domain).map(HostsLookup::Records)
        } else {
            index.lookup(&domain)
        };
        let Some(HostsLookup::Records(records)) = lookup else {
            continue;
        };
        known_name = true;
        for record in records {
            let answer = match (query_type, record) {
                (RecordType::A, HostsRecord::Address(address)) if address.is_ipv4() => {
                    Some(Record::from_rdata(
                        name.clone(),
                        ttl,
                        RData::A(A(match address {
                            std::net::IpAddr::V4(address) => *address,
                            std::net::IpAddr::V6(_) => unreachable!("IPv4 address checked"),
                        })),
                    ))
                }
                (RecordType::AAAA, HostsRecord::Address(address)) if address.is_ipv6() => {
                    Some(Record::from_rdata(
                        name.clone(),
                        ttl,
                        RData::AAAA(AAAA(match address {
                            std::net::IpAddr::V6(address) => *address,
                            std::net::IpAddr::V4(_) => unreachable!("IPv6 address checked"),
                        })),
                    ))
                }
                (RecordType::CNAME, HostsRecord::Cname(target)) => {
                    Name::from_ascii(target.as_str()).ok().map(|target| {
                        Record::from_rdata(name.clone(), ttl, RData::CNAME(CNAME(target)))
                    })
                }
                _ => None,
            };
            if let Some(answer) = answer {
                answers.push(answer);
            }
        }
    }
    (answers, known_name)
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
    use crate::resource::HostsIndex;

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

    fn request_for(id: u16, name: &str, record_type: RecordType) -> DnsRequest {
        let mut request = request(id);
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(Name::from_ascii(name).unwrap(), record_type));
        request.query = CanonicalQuery::from_message(message).unwrap();
        request
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

    #[tokio::test]
    async fn resource_hosts_core_returns_cname_and_wildcard_answers() {
        let index = HostsIndex::parse_json(
            r#"{
                "alias.example": {"CNAME": "target.example"},
                "*.wild.example": {"A": "192.0.2.7"}
            }"#,
        )
        .unwrap();
        let core = HostsCore::from_resource_indexes(vec![index], 60);

        let cname = core
            .resolve(&request_for(7, "alias.example.", RecordType::CNAME))
            .await
            .unwrap();
        let CoreOutcome::Response(cname) = cname else {
            panic!("expected CNAME response");
        };
        assert_eq!(cname.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(cname.as_message().answers.len(), 1);
        assert_eq!(
            cname.as_message().answers[0].record_type(),
            RecordType::CNAME
        );

        let wildcard = core
            .resolve(&request_for(8, "child.wild.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(wildcard) = wildcard else {
            panic!("expected wildcard response");
        };
        assert_eq!(wildcard.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(wildcard.as_message().answers.len(), 1);
        assert_eq!(
            wildcard.as_message().answers[0].record_type(),
            RecordType::A
        );
    }

    #[tokio::test]
    async fn resource_hosts_core_preserves_nodata_and_nxdomain_semantics() {
        let index = HostsIndex::parse_hosts("192.0.2.1 known.example\n").unwrap();
        let core = HostsCore::from_resource_indexes(vec![index], 60);

        let nodata = core
            .resolve(&request_for(7, "known.example.", RecordType::AAAA))
            .await
            .unwrap();
        let CoreOutcome::Response(nodata) = nodata else {
            panic!("expected NODATA response");
        };
        assert_eq!(nodata.class(), crate::dns::ResponseClass::NoData);

        let nxdomain = core
            .resolve(&request_for(8, "missing.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(nxdomain) = nxdomain else {
            panic!("expected NXDOMAIN response");
        };
        assert_eq!(nxdomain.class(), crate::dns::ResponseClass::NxDomain);
    }
}
