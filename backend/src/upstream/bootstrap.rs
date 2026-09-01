//! DoH 目标地址选择、bootstrap 查询与 TTL 状态。
//!
//! `BootstrapResolver` 只通过调用方注入的 `DnsExchange` 执行 A/AAAA 查询，
//! 不会偷偷调用 system resolver；`AddressResolutionState` 负责 connect_ip、
//! bootstrap 缓存和 system resolver 之间的显式优先级。

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query},
    rr::{Name, RData, RecordType},
};
use thiserror::Error;

use crate::config::resolve::{ConfigId, ResolvedUpstream};
use crate::dns::{
    CancelReason, CanonicalQuery, CanonicalResponse, RequestContext, ResponseClass, TtlMetadata,
};
use crate::ports::exchange::{DnsExchange, TransportFailureClass, UpstreamOutcome};

/// Bootstrap TTL 的默认实现边界。
pub const DEFAULT_BOOTSTRAP_MIN_TTL: Duration = Duration::from_secs(5);
pub const DEFAULT_BOOTSTRAP_MAX_TTL: Duration = Duration::from_secs(3_600);

/// TTL 缓存的实现级边界。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressCachePolicy {
    min_ttl: Duration,
    max_ttl: Duration,
}

impl AddressCachePolicy {
    pub fn new(min_ttl: Duration, max_ttl: Duration) -> Result<Self, CachePolicyError> {
        if max_ttl.is_zero() || min_ttl > max_ttl {
            return Err(CachePolicyError);
        }
        Ok(Self { min_ttl, max_ttl })
    }

    pub const fn defaults() -> Self {
        Self {
            min_ttl: DEFAULT_BOOTSTRAP_MIN_TTL,
            max_ttl: DEFAULT_BOOTSTRAP_MAX_TTL,
        }
    }

    pub const fn min_ttl(self) -> Duration {
        self.min_ttl
    }

    pub const fn max_ttl(self) -> Duration {
        self.max_ttl
    }

    fn bound(self, ttl: Duration) -> Duration {
        ttl.max(self.min_ttl).min(self.max_ttl)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("bootstrap TTL cache policy is invalid")]
pub struct CachePolicyError;

/// 一次 bootstrap A/AAAA 答案的纯数据表示。
#[derive(Clone, Eq, PartialEq)]
pub struct BootstrapAnswer {
    addresses: Arc<[IpAddr]>,
    ttl: Duration,
}

impl fmt::Debug for BootstrapAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapAnswer")
            .field("address_count", &self.addresses.len())
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl BootstrapAnswer {
    pub fn new(addresses: Vec<IpAddr>, ttl: Duration) -> Result<Self, AnswerError> {
        let addresses = normalize_addresses(addresses)?;
        Ok(Self { addresses, ttl })
    }

    pub fn from_ttl_metadata(
        addresses: Vec<IpAddr>,
        ttl: TtlMetadata,
    ) -> Result<Self, AnswerError> {
        let seconds = ttl.min_ttl.ok_or(AnswerError::MissingTtl)?;
        Self::new(addresses, Duration::from_secs(u64::from(seconds)))
    }

    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }

    pub const fn ttl(&self) -> Duration {
        self.ttl
    }
}

/// 从已完成 wire/question 校验的 DNS 响应提取 bootstrap A/AAAA 答案。
///
/// 这里只消费 canonical response，不执行查询，也不会把 system resolver
/// 当作 bootstrap 失败后的隐式回退。
pub fn bootstrap_answer_from_response(
    response: &CanonicalResponse,
) -> Result<BootstrapAnswer, BootstrapResponseError> {
    if !matches!(response.class(), ResponseClass::Positive) {
        return Err(BootstrapResponseError::NonPositive {
            class: response.class(),
        });
    }
    let question_name = &response.as_message().queries[0].name;
    let address_records: Vec<(std::net::IpAddr, u32)> = response
        .as_message()
        .answers
        .iter()
        .filter(|record| record.name == *question_name)
        .filter_map(|record| match &record.data {
            RData::A(address) => Some((std::net::IpAddr::V4(address.0), record.ttl)),
            RData::AAAA(address) => Some((std::net::IpAddr::V6(address.0), record.ttl)),
            _ => None,
        })
        .collect();
    let ttl = address_records
        .iter()
        .map(|(_, ttl)| *ttl)
        .min()
        .ok_or(BootstrapResponseError::Answer(AnswerError::Empty))?;
    let addresses = address_records
        .into_iter()
        .map(|(address, _)| address)
        .collect();
    BootstrapAnswer::new(addresses, Duration::from_secs(u64::from(ttl)))
        .map_err(BootstrapResponseError::Answer)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum BootstrapResponseError {
    #[error("bootstrap response is not a positive DNS answer: {class:?}")]
    NonPositive { class: ResponseClass },
    #[error("bootstrap response contains no usable A or AAAA address: {0}")]
    Answer(AnswerError),
}

/// 通过指定 connector 执行 bootstrap A/AAAA 查询的 resolver。
///
/// resolver 不负责选择 bootstrap connector，也不接入 `PolicyCore`，避免把
/// bootstrap 解析再次路由回依赖该 bootstrap 的 DoH upstream。
#[derive(Clone)]
pub struct BootstrapResolver {
    connector: Arc<dyn DnsExchange>,
}

impl fmt::Debug for BootstrapResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapResolver")
            .field("connector", &self.connector.connector_id())
            .finish()
    }
}

impl BootstrapResolver {
    pub fn new(connector: Arc<dyn DnsExchange>) -> Self {
        Self { connector }
    }

    pub fn connector_id(&self) -> &crate::ports::exchange::ConnectorId {
        self.connector.connector_id()
    }

    /// 顺序查询 A、AAAA，并合并所有合法地址；取消会立即终止本次解析。
    pub async fn resolve(
        &self,
        host: &str,
        context: &RequestContext,
    ) -> Result<BootstrapAnswer, BootstrapResolverError> {
        let queries = [
            bootstrap_query(host, RecordType::A)?,
            bootstrap_query(host, RecordType::AAAA)?,
        ];
        let connector = self.connector_id().as_str().to_owned();
        let mut answers = Vec::new();
        let mut transport_failure = None;

        for query in &queries {
            match self.connector.exchange(query, context).await {
                UpstreamOutcome::Response(response) => {
                    if let Ok(answer) = bootstrap_answer_from_response(&response) {
                        answers.push(answer);
                    }
                }
                UpstreamOutcome::TransportFailure(failure) => {
                    if transport_failure.is_none() {
                        transport_failure = Some(failure.class);
                    }
                }
                UpstreamOutcome::Cancelled(reason) => {
                    return Err(BootstrapResolverError::Cancelled { connector, reason });
                }
            }
        }

        if !answers.is_empty() {
            let ttl = answers
                .iter()
                .map(BootstrapAnswer::ttl)
                .min()
                .expect("non-empty bootstrap answer list has a minimum TTL");
            let addresses = answers
                .iter()
                .flat_map(|answer| answer.addresses().iter().copied())
                .collect();
            return BootstrapAnswer::new(addresses, ttl).map_err(|_| {
                BootstrapResolverError::NoAddress {
                    connector: connector.clone(),
                }
            });
        }

        if let Some(class) = transport_failure {
            return Err(BootstrapResolverError::Transport { connector, class });
        }
        Err(BootstrapResolverError::NoAddress { connector })
    }
}

fn bootstrap_query(
    host: &str,
    record_type: RecordType,
) -> Result<CanonicalQuery, BootstrapResolverError> {
    let name = Name::from_str(host).map_err(|_| BootstrapResolverError::InvalidName)?;
    let mut message = Message::new(0, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(name, record_type));
    CanonicalQuery::from_message(message).map_err(|_| BootstrapResolverError::QueryBuild)
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum BootstrapResolverError {
    #[error("bootstrap resolver query name is invalid")]
    InvalidName,
    #[error("bootstrap resolver query could not be constructed")]
    QueryBuild,
    #[error("bootstrap resolver connector `{connector}` was cancelled: {reason:?}")]
    Cancelled {
        connector: String,
        reason: CancelReason,
    },
    #[error("bootstrap resolver connector `{connector}` failed: {class:?}")]
    Transport {
        connector: String,
        class: TransportFailureClass,
    },
    #[error("bootstrap resolver connector `{connector}` returned no usable address")]
    NoAddress { connector: String },
}

/// system resolver 的纯结果占位；真正的 resolver 属于 adapter/port 层。
#[derive(Clone, Eq, PartialEq)]
pub struct SystemResolverAnswer {
    addresses: Arc<[IpAddr]>,
}

impl fmt::Debug for SystemResolverAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SystemResolverAnswer")
            .field("address_count", &self.addresses.len())
            .finish()
    }
}

impl SystemResolverAnswer {
    pub fn new(addresses: Vec<IpAddr>) -> Result<Self, AnswerError> {
        Ok(Self {
            addresses: normalize_addresses(addresses)?,
        })
    }

    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum AnswerError {
    #[error("address answer is empty")]
    Empty,
    #[error("address answer contains an unusable address")]
    UnusableAddress,
    #[error("address answer has no positive TTL")]
    MissingTtl,
}

fn normalize_addresses(addresses: Vec<IpAddr>) -> Result<Arc<[IpAddr]>, AnswerError> {
    let mut normalized = Vec::with_capacity(addresses.len());
    for address in addresses {
        if address.is_unspecified() {
            return Err(AnswerError::UnusableAddress);
        }
        if !normalized.contains(&address) {
            normalized.push(address);
        }
    }
    if normalized.is_empty() {
        return Err(AnswerError::Empty);
    }
    Ok(normalized.into())
}

/// bootstrap 查询的状态必须由调用方显式提供，避免失败后偷偷走 system resolver。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BootstrapResolution {
    /// 本次没有发起 bootstrap 查询，可使用仍在 TTL 内的旧缓存。
    NotAttempted,
    /// 本次查询得到完整地址答案。
    Answer(BootstrapAnswer),
    /// 本次查询失败；仍可在 TTL 内复用旧地址，但结果会标记 degraded。
    Failed,
}

/// system resolver 的结果占位，不包含任何实际 I/O 行为。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SystemResolverResolution {
    Answer(SystemResolverAnswer),
    Failed,
}

/// 从已解析配置抽取的、可供地址状态机消费的请求。
#[derive(Clone, Eq, PartialEq)]
pub struct AddressResolutionRequest {
    upstream_id: ConfigId,
    connect_ip: Option<IpAddr>,
    bootstrap: Option<ConfigId>,
}

impl fmt::Debug for AddressResolutionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddressResolutionRequest")
            .field("upstream_id", &self.upstream_id)
            .field("has_connect_ip", &self.connect_ip.is_some())
            .field("bootstrap", &self.bootstrap)
            .finish()
    }
}

impl AddressResolutionRequest {
    pub fn from_resolved(upstream: &ResolvedUpstream) -> Result<Self, AddressResolutionError> {
        let ResolvedUpstream::Doh {
            id,
            bootstrap,
            connect_ip,
            ..
        } = upstream
        else {
            return Err(AddressResolutionError::NotDoh {
                upstream: upstream_id(upstream),
            });
        };

        Ok(Self {
            upstream_id: id.clone(),
            connect_ip: *connect_ip,
            bootstrap: bootstrap.clone(),
        })
    }

    pub fn upstream_id(&self) -> &ConfigId {
        &self.upstream_id
    }

    pub fn connect_ip(&self) -> Option<IpAddr> {
        self.connect_ip
    }

    pub fn bootstrap(&self) -> Option<&ConfigId> {
        self.bootstrap.as_ref()
    }
}

/// 地址来源；bootstrap 缓存命中与新答案使用同一来源，degraded 单独表达旧地址复用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AddressSource {
    ExplicitConnectIp,
    Bootstrap(ConfigId),
    SystemResolver,
}

/// 已选择的连接地址集合。
#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedAddresses {
    addresses: Arc<[IpAddr]>,
    source: AddressSource,
    expires_at: Option<Instant>,
    degraded: bool,
}

impl fmt::Debug for ResolvedAddresses {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedAddresses")
            .field("address_count", &self.addresses.len())
            .field("source", &self.source)
            .field("has_expiry", &self.expires_at.is_some())
            .field("degraded", &self.degraded)
            .finish()
    }
}

impl ResolvedAddresses {
    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }

    pub fn source(&self) -> &AddressSource {
        &self.source
    }

    pub const fn expires_at(&self) -> Option<Instant> {
        self.expires_at
    }

    pub const fn is_degraded(&self) -> bool {
        self.degraded
    }
}

#[derive(Clone)]
struct CachedAddresses {
    addresses: Arc<[IpAddr]>,
    expires_at: Instant,
}

impl fmt::Debug for CachedAddresses {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachedAddresses")
            .field("address_count", &self.addresses.len())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// bootstrap/connect_ip/system resolver 的地址优先级与 TTL 状态机。
#[derive(Clone, Debug)]
pub struct AddressResolutionState {
    policy: AddressCachePolicy,
    cache: BTreeMap<ConfigId, CachedAddresses>,
}

impl AddressResolutionState {
    pub fn new(policy: AddressCachePolicy) -> Self {
        Self {
            policy,
            cache: BTreeMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(AddressCachePolicy::defaults())
    }

    pub const fn policy(&self) -> AddressCachePolicy {
        self.policy
    }

    pub fn cached_entry_count(&self) -> usize {
        self.cache.len()
    }

    /// 选择地址的严格顺序为 connect_ip > bootstrap > system resolver。
    pub fn resolve(
        &mut self,
        request: &AddressResolutionRequest,
        bootstrap: BootstrapResolution,
        system: SystemResolverResolution,
        now: Instant,
    ) -> Result<ResolvedAddresses, AddressResolutionError> {
        if let Some(connect_ip) = request.connect_ip {
            return Ok(ResolvedAddresses {
                addresses: vec![connect_ip].into(),
                source: AddressSource::ExplicitConnectIp,
                expires_at: None,
                degraded: false,
            });
        }

        if let Some(bootstrap_id) = request.bootstrap.as_ref() {
            return self.resolve_bootstrap(request, bootstrap_id, bootstrap, now);
        }

        match system {
            SystemResolverResolution::Answer(answer) => Ok(ResolvedAddresses {
                addresses: answer.addresses.clone(),
                source: AddressSource::SystemResolver,
                expires_at: None,
                degraded: false,
            }),
            SystemResolverResolution::Failed => Err(AddressResolutionError::NoAddress {
                upstream: request.upstream_id.as_str().to_owned(),
                reason: NoAddressReason::SystemResolverUnavailable,
            }),
        }
    }

    pub fn clear(&mut self, upstream_id: &ConfigId) {
        self.cache.remove(upstream_id);
    }

    pub fn purge_expired(&mut self, now: Instant) {
        self.cache.retain(|_, entry| now < entry.expires_at);
    }

    fn resolve_bootstrap(
        &mut self,
        request: &AddressResolutionRequest,
        bootstrap_id: &ConfigId,
        resolution: BootstrapResolution,
        now: Instant,
    ) -> Result<ResolvedAddresses, AddressResolutionError> {
        match resolution {
            BootstrapResolution::Answer(answer) => {
                let ttl = self.policy.bound(answer.ttl());
                let expires_at = now.checked_add(ttl).unwrap_or(now);
                self.cache.insert(
                    request.upstream_id.clone(),
                    CachedAddresses {
                        addresses: answer.addresses.clone(),
                        expires_at,
                    },
                );
                Ok(ResolvedAddresses {
                    addresses: answer.addresses,
                    source: AddressSource::Bootstrap(bootstrap_id.clone()),
                    expires_at: Some(expires_at),
                    degraded: false,
                })
            }
            BootstrapResolution::NotAttempted => {
                self.cached_or_error(request, bootstrap_id, now, false)
            }
            BootstrapResolution::Failed => self.cached_or_error(request, bootstrap_id, now, true),
        }
    }

    fn cached_or_error(
        &mut self,
        request: &AddressResolutionRequest,
        bootstrap_id: &ConfigId,
        now: Instant,
        degraded: bool,
    ) -> Result<ResolvedAddresses, AddressResolutionError> {
        let Some(entry) = self.cache.get(&request.upstream_id).cloned() else {
            return Err(AddressResolutionError::NoAddress {
                upstream: request.upstream_id.as_str().to_owned(),
                reason: NoAddressReason::BootstrapUnavailable,
            });
        };
        if now >= entry.expires_at {
            self.cache.remove(&request.upstream_id);
            return Err(AddressResolutionError::NoAddress {
                upstream: request.upstream_id.as_str().to_owned(),
                reason: NoAddressReason::BootstrapExpired,
            });
        }
        Ok(ResolvedAddresses {
            addresses: entry.addresses,
            source: AddressSource::Bootstrap(bootstrap_id.clone()),
            expires_at: Some(entry.expires_at),
            degraded,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum AddressResolutionError {
    #[error("upstream `{upstream}` is not a DoH upstream")]
    NotDoh { upstream: String },
    #[error("upstream `{upstream}` has no usable address: {reason}")]
    NoAddress {
        upstream: String,
        reason: NoAddressReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoAddressReason {
    BootstrapUnavailable,
    BootstrapExpired,
    SystemResolverUnavailable,
}

impl fmt::Display for NoAddressReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BootstrapUnavailable => "bootstrap unavailable",
            Self::BootstrapExpired => "bootstrap cache expired",
            Self::SystemResolverUnavailable => "system resolver unavailable",
        })
    }
}

fn upstream_id(upstream: &ResolvedUpstream) -> String {
    match upstream {
        ResolvedUpstream::Hosts { id, .. }
        | ResolvedUpstream::Doh { id, .. }
        | ResolvedUpstream::Group { id, .. } => id.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A, rdata::AAAA};
    use url::Url;

    use crate::config::resolve::{ConfigId, ResolvedUpstream};
    use crate::dns::{
        CacheCompatibilityKey, CancelReason, Cancellation, CanonicalQuery, ClientIdentity,
        Deadline, DnsMessageId, ListenerId, RequestContext, RequestId, RequestMeta,
        RuntimeRevision, TransportCapabilities, TransportClass,
    };
    use crate::ports::exchange::{
        ConnectorId, DnsExchange, TransportFailure, TransportFailureClass, UpstreamOutcome,
    };
    use crate::ports::testing::FakeExchange;
    use crate::upstream::HostsExchange;

    use super::{
        AddressCachePolicy, AddressResolutionError, AddressResolutionRequest,
        AddressResolutionState, AddressSource, BootstrapAnswer, BootstrapResolution,
        BootstrapResolver, BootstrapResolverError, BootstrapResponseError, NoAddressReason,
        ResolvedAddresses, SystemResolverAnswer, SystemResolverResolution,
        bootstrap_answer_from_response,
    };

    fn id(value: &str) -> ConfigId {
        ConfigId::new(value).unwrap()
    }

    fn doh(name: &str, connect_ip: Option<IpAddr>, bootstrap: Option<&str>) -> ResolvedUpstream {
        ResolvedUpstream::Doh {
            id: id(name),
            address: Url::parse("https://dns.example.test/dns-query").unwrap(),
            bootstrap: bootstrap.map(id),
            connect_ip,
            proxy: None,
            edns_client_subnet: None,
        }
    }

    fn request(upstream: &ResolvedUpstream) -> AddressResolutionRequest {
        AddressResolutionRequest::from_resolved(upstream).unwrap()
    }

    fn context(cancellation: Cancellation) -> RequestContext {
        let now = Instant::now();
        RequestContext {
            meta: RequestMeta {
                request_id: RequestId(1),
                trace_id: None,
                received_at: now,
                received_at_utc: SystemTime::now(),
                deadline: Deadline::new(now + Duration::from_secs(30)),
                cancellation,
                connection_id: None,
                stream_id: None,
                listener_id: ListenerId::from("bootstrap-test"),
                route_id: None,
                original_dns_id: None,
            },
            client: ClientIdentity::default(),
            transport: TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            runtime_revision: RuntimeRevision(1),
        }
    }

    fn answer(address: [u8; 4], ttl: u64) -> BootstrapResolution {
        BootstrapResolution::Answer(
            BootstrapAnswer::new(
                vec![IpAddr::V4(Ipv4Addr::from(address))],
                Duration::from_secs(ttl),
            )
            .unwrap(),
        )
    }

    fn response(records: Vec<Record>, record_type: RecordType) -> crate::dns::CanonicalResponse {
        let name = Name::from_str("resolver.example.").unwrap();
        let mut query_message = Message::new(1, MessageType::Query, OpCode::Query);
        query_message.add_query(Query::query(name.clone(), record_type));
        let query = CanonicalQuery::from_message(query_message).unwrap();

        let mut response_message = Message::new(2, MessageType::Response, OpCode::Query);
        response_message.metadata.response_code = ResponseCode::NoError;
        response_message.add_query(Query::query(name, record_type));
        response_message.add_answers(records);
        crate::dns::CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(2))
            .unwrap()
    }

    fn empty_response(code: ResponseCode) -> crate::dns::CanonicalResponse {
        let name = Name::from_str("resolver.example.").unwrap();
        let mut query_message = Message::new(1, MessageType::Query, OpCode::Query);
        query_message.add_query(Query::query(name.clone(), RecordType::A));
        let query = CanonicalQuery::from_message(query_message).unwrap();

        let mut response_message = Message::new(2, MessageType::Response, OpCode::Query);
        response_message.metadata.response_code = code;
        response_message.add_query(Query::query(name, RecordType::A));
        crate::dns::CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(2))
            .unwrap()
    }

    #[tokio::test]
    async fn resolver_queries_a_and_aaaa_and_merges_addresses() {
        let connector = Arc::new(
            HostsExchange::from_resolved(&ResolvedUpstream::Hosts {
                id: id("bootstrap"),
                format: "hosts".to_owned(),
                hosts: "192.0.2.10 resolver.example.\n2001:db8::10 resolver.example.\n".to_owned(),
            })
            .unwrap(),
        );
        let resolver = BootstrapResolver::new(connector);

        let answer = resolver
            .resolve("resolver.example.", &context(Cancellation::new()))
            .await
            .unwrap();

        assert_eq!(
            answer.addresses(),
            &[
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
                IpAddr::V6("2001:db8::10".parse().unwrap()),
            ]
        );
        assert_eq!(answer.ttl(), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn resolver_returns_transport_failure_when_no_family_has_an_answer() {
        let connector = Arc::new(FakeExchange::new(ConnectorId::new("bootstrap").unwrap()));
        for _ in 0..2 {
            connector
                .push(UpstreamOutcome::TransportFailure(TransportFailure {
                    connector: connector.connector_id().clone(),
                    class: TransportFailureClass::Timeout,
                    retryable: true,
                    safe_context: Some("test"),
                }))
                .unwrap();
        }
        let resolver = BootstrapResolver::new(connector.clone());

        assert_eq!(
            resolver
                .resolve("resolver.example.", &context(Cancellation::new()))
                .await,
            Err(BootstrapResolverError::Transport {
                connector: "bootstrap".to_owned(),
                class: TransportFailureClass::Timeout,
            })
        );
        assert_eq!(connector.calls(), 2);
    }

    #[tokio::test]
    async fn resolver_stops_on_cancellation_without_querying_aaaa() {
        let connector = Arc::new(FakeExchange::new(ConnectorId::new("bootstrap").unwrap()));
        connector
            .push(UpstreamOutcome::Cancelled(CancelReason::Shutdown))
            .unwrap();
        let resolver = BootstrapResolver::new(connector.clone());

        assert_eq!(
            resolver
                .resolve("resolver.example.", &context(Cancellation::new()))
                .await,
            Err(BootstrapResolverError::Cancelled {
                connector: "bootstrap".to_owned(),
                reason: CancelReason::Shutdown,
            })
        );
        assert_eq!(connector.calls(), 1);
    }

    #[tokio::test]
    async fn resolver_rejects_invalid_host_before_exchange() {
        let connector = Arc::new(FakeExchange::new(ConnectorId::new("bootstrap").unwrap()));
        let resolver = BootstrapResolver::new(connector.clone());

        assert_eq!(
            resolver
                .resolve("not a dns name", &context(Cancellation::new()))
                .await,
            Err(BootstrapResolverError::InvalidName)
        );
        assert_eq!(connector.calls(), 0);
    }

    #[test]
    fn extracts_a_aaaa_addresses_and_lowest_ttl_from_positive_response() {
        let name = Name::from_str("resolver.example.").unwrap();
        let response = response(
            vec![
                Record::from_rdata(name.clone(), 45, RData::A(A(Ipv4Addr::new(192, 0, 2, 10)))),
                Record::from_rdata(name, 30, RData::AAAA(AAAA("2001:db8::10".parse().unwrap()))),
                Record::from_rdata(
                    Name::from_str("other.example.").unwrap(),
                    1,
                    RData::A(A(Ipv4Addr::new(192, 0, 2, 99))),
                ),
            ],
            RecordType::A,
        );

        let answer = bootstrap_answer_from_response(&response).unwrap();
        assert_eq!(
            answer.addresses(),
            &[
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
                IpAddr::V6("2001:db8::10".parse().unwrap()),
            ]
        );
        assert_eq!(answer.ttl(), Duration::from_secs(30));
    }

    #[test]
    fn rejects_nonpositive_response_as_bootstrap_answer() {
        let response = empty_response(ResponseCode::ServFail);
        assert!(matches!(
            bootstrap_answer_from_response(&response),
            Err(BootstrapResponseError::NonPositive {
                class: crate::dns::ResponseClass::ServFail
            })
        ));
    }

    #[test]
    fn explicit_connect_ip_has_priority_over_failed_bootstrap_and_system() {
        let upstream = doh(
            "remote",
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            Some("bootstrap"),
        );
        let mut state = AddressResolutionState::with_defaults();
        let result = state
            .resolve(
                &request(&upstream),
                BootstrapResolution::Failed,
                SystemResolverResolution::Answer(
                    SystemResolverAnswer::new(vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))])
                        .unwrap(),
                ),
                Instant::now(),
            )
            .unwrap();

        assert_eq!(result.source(), &AddressSource::ExplicitConnectIp);
        assert_eq!(result.addresses(), &[IpAddr::V6(Ipv6Addr::LOCALHOST)]);
        assert!(!result.is_degraded());
    }

    #[test]
    fn bootstrap_answer_is_cached_with_bounded_ttl_and_expires() {
        let upstream = doh("remote", None, Some("bootstrap"));
        let now = Instant::now();
        let policy =
            AddressCachePolicy::new(Duration::from_secs(10), Duration::from_secs(60)).unwrap();
        let mut state = AddressResolutionState::new(policy);
        let req = request(&upstream);

        let fresh = state
            .resolve(
                &req,
                answer([192, 0, 2, 10], 1),
                SystemResolverResolution::Failed,
                now,
            )
            .unwrap();
        assert_eq!(fresh.expires_at(), Some(now + Duration::from_secs(10)));

        let cached = state
            .resolve(
                &req,
                BootstrapResolution::NotAttempted,
                SystemResolverResolution::Failed,
                now + Duration::from_secs(9),
            )
            .unwrap();
        assert_eq!(cached.addresses(), fresh.addresses());
        assert!(!cached.is_degraded());

        let expired = state.resolve(
            &req,
            BootstrapResolution::NotAttempted,
            SystemResolverResolution::Answer(
                SystemResolverAnswer::new(vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 99))]).unwrap(),
            ),
            now + Duration::from_secs(10),
        );
        assert!(matches!(
            expired,
            Err(AddressResolutionError::NoAddress {
                reason: NoAddressReason::BootstrapExpired,
                ..
            })
        ));
    }

    #[test]
    fn failed_refresh_reuses_unexpired_address_as_degraded() {
        let upstream = doh("remote", None, Some("bootstrap"));
        let now = Instant::now();
        let mut state = AddressResolutionState::new(
            AddressCachePolicy::new(Duration::from_secs(1), Duration::from_secs(30)).unwrap(),
        );
        let req = request(&upstream);
        state
            .resolve(
                &req,
                answer([192, 0, 2, 10], 10),
                SystemResolverResolution::Failed,
                now,
            )
            .unwrap();

        let degraded = state
            .resolve(
                &req,
                BootstrapResolution::Failed,
                SystemResolverResolution::Failed,
                now + Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(
            degraded.addresses(),
            &[IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))]
        );
        assert!(degraded.is_degraded());
    }

    #[test]
    fn configured_bootstrap_failure_does_not_fall_back_to_system_resolver() {
        let upstream = doh("remote", None, Some("bootstrap"));
        let mut state = AddressResolutionState::with_defaults();
        let result = state.resolve(
            &request(&upstream),
            BootstrapResolution::Failed,
            SystemResolverResolution::Answer(
                SystemResolverAnswer::new(vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20))]).unwrap(),
            ),
            Instant::now(),
        );
        assert!(matches!(
            result,
            Err(AddressResolutionError::NoAddress {
                reason: NoAddressReason::BootstrapUnavailable,
                ..
            })
        ));
    }

    #[test]
    fn system_resolver_is_used_only_without_bootstrap() {
        let upstream = doh("remote", None, None);
        let mut state = AddressResolutionState::with_defaults();
        let result = state
            .resolve(
                &request(&upstream),
                BootstrapResolution::NotAttempted,
                SystemResolverResolution::Answer(
                    SystemResolverAnswer::new(vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 30))])
                        .unwrap(),
                ),
                Instant::now(),
            )
            .unwrap();
        assert_eq!(result.source(), &AddressSource::SystemResolver);
    }

    #[test]
    fn debug_output_redacts_address_values() {
        let upstream = doh("remote", None, Some("bootstrap"));
        let req = request(&upstream);
        let answer = BootstrapAnswer::new(
            vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))],
            Duration::from_secs(30),
        )
        .unwrap();
        let mut state = AddressResolutionState::with_defaults();
        let result = state
            .resolve(
                &req,
                BootstrapResolution::Answer(answer.clone()),
                SystemResolverResolution::Failed,
                Instant::now(),
            )
            .unwrap();

        let answer_debug = format!("{answer:?}");
        let request_debug = format!("{req:?}");
        let state_debug = format!("{state:?}");
        let result_debug = format!("{result:?}");
        for debug in [answer_debug, request_debug, state_debug, result_debug] {
            assert!(!debug.contains("203.0.113.7"));
            assert!(!debug.contains("dns.example.test"));
        }
    }

    #[test]
    fn rejects_non_doh_upstream_at_request_boundary() {
        let upstream = ResolvedUpstream::Hosts {
            id: id("local"),
            format: "hosts".to_owned(),
            hosts: "192.0.2.1 example.test".to_owned(),
        };
        assert!(matches!(
            AddressResolutionRequest::from_resolved(&upstream),
            Err(AddressResolutionError::NotDoh { upstream }) if upstream == "local"
        ));
    }

    #[test]
    fn result_debug_is_address_count_only() {
        let result = ResolvedAddresses {
            addresses: vec![IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9))].into(),
            source: AddressSource::SystemResolver,
            expires_at: None,
            degraded: false,
        };
        assert_eq!(
            format!("{result:?}"),
            "ResolvedAddresses { address_count: 1, source: SystemResolver, has_expiry: false, degraded: false }"
        );
    }
}
