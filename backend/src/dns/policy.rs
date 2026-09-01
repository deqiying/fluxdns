//! PolicyIndex 驱动的 DNS Core 首轮接线。
//!
//! 本 core 先处理已编译的本地 hosts，再执行当前已支持的 hosts/group upstream。
//! 尚未具备真实 connector 的分支保持确定性的 SERVFAIL，不伪造网络结果。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use hickory_proto::op::ResponseCode;
use thiserror::Error;

use crate::config::resolve::{ConfigId, ResolvedConfig, ResolvedHostsResource, ResolvedUpstream};
use crate::policy::{PolicyBuildError, PolicyIndex, PolicyRequest};
use crate::ports::PortFuture;
use crate::ports::exchange::{DnsExchange, UpstreamOutcome};
use crate::resource::{CanonicalDomain, HostsIndex, HostsLimits, ResourceLoadError, load_hosts};
use crate::upstream::{GroupSelector, RegistryError, UpstreamGroupExecutor, UpstreamRegistry};

use super::handler::resource_answers;
use super::{CanonicalResponse, CoreError, CoreOutcome, DnsCore, DnsRequest};

#[derive(Debug, Error)]
pub enum PolicyCoreBuildError {
    #[error("policy index could not be built: {0:?}")]
    Policy(PolicyBuildError),
    #[error("hosts resource `{resource}` could not be loaded: {source}")]
    HostsLoad {
        resource: String,
        #[source]
        source: ResourceLoadError,
    },
    #[error("upstream `{upstream}` could not be built: {reason}")]
    Upstream { upstream: String, reason: String },
}

/// 使用同一份 resolved config 构建 policy/resource 本地回答 core。
#[derive(Clone, Debug)]
pub struct PolicyDnsCore {
    policy: PolicyIndex,
    hosts: BTreeMap<ConfigId, Arc<HostsIndex>>,
    upstreams: UpstreamRuntime,
    ttl: u32,
}

impl PolicyDnsCore {
    pub fn from_config(config: &ResolvedConfig, ttl: u32) -> Result<Self, PolicyCoreBuildError> {
        let direct_upstreams = direct_upstreams(&config.upstreams);
        let registry =
            UpstreamRegistry::from_resolved_with_outbounds(&direct_upstreams, &config.outbounds)
                .map_err(|error| {
                    let error = registry_build_error(error);
                    PolicyCoreBuildError::Upstream {
                        upstream: error.upstream,
                        reason: error.reason,
                    }
                })?;
        Self::from_config_with_registry(config, ttl, registry)
    }

    pub(crate) fn from_config_with_registry(
        config: &ResolvedConfig,
        ttl: u32,
        registry: UpstreamRegistry,
    ) -> Result<Self, PolicyCoreBuildError> {
        let upstreams =
            UpstreamRuntime::from_registry(&config.upstreams, registry).map_err(|error| {
                PolicyCoreBuildError::Upstream {
                    upstream: error.upstream,
                    reason: error.reason,
                }
            })?;
        Self::from_config_with_upstream_runtime(config, ttl, upstreams)
    }

    fn from_config_with_upstream_runtime(
        config: &ResolvedConfig,
        ttl: u32,
        upstreams: UpstreamRuntime,
    ) -> Result<Self, PolicyCoreBuildError> {
        let policy = PolicyIndex::from_config(config).map_err(PolicyCoreBuildError::Policy)?;
        let mut hosts = BTreeMap::new();
        for resource in &config.hosts {
            let id = match resource {
                ResolvedHostsResource::Const { id, .. }
                | ResolvedHostsResource::File { id, .. } => id,
            };
            let loaded = load_hosts(resource, HostsLimits::default()).map_err(|source| {
                PolicyCoreBuildError::HostsLoad {
                    resource: id.as_str().to_owned(),
                    source,
                }
            })?;
            if !loaded.index().is_empty() {
                hosts.insert(id.clone(), Arc::new(loaded.index().clone()));
            }
        }
        Ok(Self {
            policy,
            hosts,
            upstreams,
            ttl,
        })
    }

    pub fn policy(&self) -> &PolicyIndex {
        &self.policy
    }

    pub fn host_resource_count(&self) -> usize {
        self.hosts.len()
    }

    pub fn upstream_count(&self) -> usize {
        self.upstreams.len()
    }
}

impl DnsCore for PolicyDnsCore {
    fn resolve<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, Result<CoreOutcome, CoreError>> {
        Box::pin(async move {
            let meta = &request.context.meta;
            if meta.cancellation.is_cancelled() || meta.deadline.is_expired(Instant::now()) {
                return Ok(CoreOutcome::NoResponse);
            }

            let Some(listener_id) = ConfigId::new(meta.listener_id.as_ref().to_owned()).ok() else {
                return servfail(request);
            };
            let qname = match CanonicalDomain::parse(&request.query.question().name().to_ascii()) {
                Ok(qname) => qname,
                Err(_) => return servfail(request),
            };
            let doh_path = reconstructed_doh_path(request);
            let plan = match self.policy.evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: doh_path.as_deref(),
                client_id: request
                    .context
                    .client
                    .client_id
                    .as_ref()
                    .map(|client_id| client_id.as_str()),
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            }) {
                Ok(plan) => plan,
                Err(_error) => return servfail(request),
            };

            if let Some(resource_id) = plan.hosts {
                let Some(index) = self.hosts.get(&resource_id) else {
                    return servfail(request);
                };
                let (answers, known_name) = resource_answers(
                    std::slice::from_ref(index.as_ref()),
                    request.query.question().name(),
                    request.query.question().query_type(),
                    self.ttl,
                );
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
                return response
                    .map(CoreOutcome::Response)
                    .map_err(CoreError::ResponseConstruction);
            }

            let Some(outcome) = self
                .upstreams
                .exchange(&plan.upstream, &request.query, &request.context)
                .await
            else {
                return servfail(request);
            };
            match outcome {
                UpstreamOutcome::Response(response) if response.matches_query(&request.query) => {
                    Ok(CoreOutcome::Response(response))
                }
                UpstreamOutcome::Cancelled(_) => Ok(CoreOutcome::NoResponse),
                UpstreamOutcome::Response(_) | UpstreamOutcome::TransportFailure(_) => {
                    servfail(request)
                }
            }
        })
    }
}

#[derive(Clone)]
struct UpstreamRuntime {
    direct: BTreeMap<ConfigId, Arc<dyn DnsExchange>>,
    groups: BTreeMap<ConfigId, Arc<UpstreamGroupExecutor>>,
}

impl fmt::Debug for UpstreamRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamRuntime")
            .field("direct_count", &self.direct.len())
            .field("group_count", &self.groups.len())
            .finish()
    }
}

#[derive(Debug)]
struct UpstreamRuntimeBuildError {
    upstream: String,
    reason: String,
}

impl UpstreamRuntime {
    fn from_registry(
        upstreams: &[ResolvedUpstream],
        registry: UpstreamRegistry,
    ) -> Result<Self, UpstreamRuntimeBuildError> {
        let mut direct = BTreeMap::new();
        for upstream in upstreams.iter().filter(|upstream| {
            matches!(
                upstream,
                ResolvedUpstream::Hosts { .. } | ResolvedUpstream::Doh { .. }
            )
        }) {
            let id = upstream_id(upstream);
            let exchange = registry
                .get_by_name(id.as_str())
                .map_err(registry_build_error)?;
            if direct.insert(id.clone(), exchange).is_some() {
                return Err(UpstreamRuntimeBuildError {
                    upstream: id.as_str().to_owned(),
                    reason: "duplicate upstream id".to_owned(),
                });
            }
        }

        let mut groups = BTreeMap::new();
        for upstream in upstreams {
            let ResolvedUpstream::Group {
                id,
                upstreams: members,
                upstream_mode,
                timeout,
                fallbacks,
                fallback_upstream_mode,
                fallback_timeout,
                ..
            } = upstream
            else {
                continue;
            };
            let selector = GroupSelector::from_upstream_mode(*upstream_mode, members.clone())
                .map_err(|error| group_build_error(id, error.to_string()))?;
            let exchanges = group_member_exchanges(&direct, id, members, "primary")?;
            let executor = if fallbacks.is_empty() {
                UpstreamGroupExecutor::new_with_timeout(selector, exchanges, *timeout)
            } else {
                let fallback_mode = (*fallback_upstream_mode)
                    .ok_or_else(|| group_build_error(id, "fallback mode is missing".to_owned()))?;
                let fallback_timeout = fallback_timeout.ok_or_else(|| {
                    group_build_error(id, "fallback timeout is missing".to_owned())
                })?;
                let fallback_selector =
                    GroupSelector::from_upstream_mode(fallback_mode, fallbacks.clone())
                        .map_err(|error| group_build_error(id, error.to_string()))?;
                let fallback_exchanges =
                    group_member_exchanges(&direct, id, fallbacks, "fallback")?;
                UpstreamGroupExecutor::new_with_fallback(
                    selector,
                    exchanges,
                    *timeout,
                    fallback_selector,
                    fallback_exchanges,
                    fallback_timeout,
                )
            }
            .map_err(|error| group_build_error(id, error.to_string()))?;
            if groups.insert(id.clone(), Arc::new(executor)).is_some() {
                return Err(group_build_error(
                    id,
                    "duplicate upstream group id".to_owned(),
                ));
            }
        }

        Ok(Self { direct, groups })
    }

    fn len(&self) -> usize {
        self.direct.len() + self.groups.len()
    }

    async fn exchange(
        &self,
        upstream: &ConfigId,
        query: &super::CanonicalQuery,
        context: &super::RequestContext,
    ) -> Option<UpstreamOutcome> {
        if let Some(exchange) = self.direct.get(upstream) {
            return Some(exchange.exchange(query, context).await);
        }
        if let Some(executor) = self.groups.get(upstream) {
            return executor.execute(query, context).await.ok();
        }
        None
    }
}

fn group_member_exchanges(
    direct: &BTreeMap<ConfigId, Arc<dyn DnsExchange>>,
    group: &ConfigId,
    members: &[crate::config::resolve::ResolvedUpstreamMember],
    role: &str,
) -> Result<Vec<Arc<dyn DnsExchange>>, UpstreamRuntimeBuildError> {
    members
        .iter()
        .map(|member| {
            direct.get(&member.name).cloned().ok_or_else(|| {
                group_build_error(
                    group,
                    format!(
                        "{role} member `{}` is not a direct connector",
                        member.name.as_str()
                    ),
                )
            })
        })
        .collect()
}

fn group_build_error(group: &ConfigId, reason: String) -> UpstreamRuntimeBuildError {
    UpstreamRuntimeBuildError {
        upstream: group.as_str().to_owned(),
        reason,
    }
}

fn direct_upstreams(upstreams: &[ResolvedUpstream]) -> Vec<ResolvedUpstream> {
    upstreams
        .iter()
        .filter(|upstream| {
            matches!(
                upstream,
                ResolvedUpstream::Hosts { .. } | ResolvedUpstream::Doh { .. }
            )
        })
        .cloned()
        .collect()
}

fn upstream_id(upstream: &ResolvedUpstream) -> &ConfigId {
    match upstream {
        ResolvedUpstream::Hosts { id, .. }
        | ResolvedUpstream::Doh { id, .. }
        | ResolvedUpstream::Group { id, .. } => id,
    }
}

fn registry_build_error(error: RegistryError) -> UpstreamRuntimeBuildError {
    let upstream = match &error {
        RegistryError::InvalidConnectorId { upstream }
        | RegistryError::InvalidHosts { upstream }
        | RegistryError::InvalidDoh { upstream }
        | RegistryError::UnsupportedUpstream { upstream, .. } => upstream.clone(),
        RegistryError::InvalidOutbound { outbound, .. }
        | RegistryError::DuplicateOutbound { outbound } => outbound.clone(),
        RegistryError::MissingOutbound { upstream, .. }
        | RegistryError::InvalidOutboundCombination { upstream, .. } => upstream.clone(),
        RegistryError::DuplicateConnector { connector }
        | RegistryError::MissingConnector { connector } => connector.clone(),
    };
    UpstreamRuntimeBuildError {
        upstream,
        reason: error.to_string(),
    }
}

fn reconstructed_doh_path(request: &DnsRequest) -> Option<String> {
    let route = request.context.meta.route_id.as_ref()?;
    let mut path = route.as_ref().to_owned();
    if path.contains("{client_id}") {
        let client_id = request.context.client.client_id.as_ref()?;
        path = path.replace("{client_id}", client_id.as_str());
    }
    Some(path)
}

fn servfail(request: &DnsRequest) -> Result<CoreOutcome, CoreError> {
    CanonicalResponse::empty_response(&request.query, ResponseCode::ServFail)
        .map(CoreOutcome::Response)
        .map_err(CoreError::ResponseConstruction)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use crate::config::model::EcsMode;
    use crate::config::resolve::{
        ConfigId, ResolvedEcs, ResolvedOutbound, ResolvedSecretRef, ResolvedUpstream, ValueSource,
    };
    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, CoreOutcome, Deadline, DnsCore,
        DnsRequest, ListenerId, RequestContext, RequestId, RequestMeta, RuntimeRevision,
        TransportCapabilities, TransportClass,
    };
    use crate::ports::{PortError, PortFuture};
    use crate::upstream::{DohHttpRequest, DohHttpResponseOwned, DohHttpTransport};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{PolicyDnsCore, UpstreamRuntime};
    use crate::upstream::UpstreamRegistry;

    struct FakeDohTransport {
        request: Mutex<Option<DohHttpRequest>>,
        response: Vec<u8>,
    }

    struct ServFailDohTransport;

    impl DohHttpTransport for ServFailDohTransport {
        fn post<'a>(
            &'a self,
            request: DohHttpRequest,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
            Box::pin(async move {
                let query = Message::from_vec(request.body()).unwrap();
                let mut response =
                    Message::new(query.metadata.id, MessageType::Response, OpCode::Query);
                response.metadata.response_code = ResponseCode::ServFail;
                response.add_query(query.queries[0].clone());
                Ok(DohHttpResponseOwned {
                    status: 200,
                    content_type: Some("application/dns-message".to_owned()),
                    body: response.to_vec().unwrap(),
                })
            })
        }
    }

    impl FakeDohTransport {
        fn response() -> Vec<u8> {
            let mut message = Message::new(1, MessageType::Response, OpCode::Query);
            message.metadata.response_code = ResponseCode::NoError;
            message.add_query(Query::query(
                Name::from_str("remote.example.").unwrap(),
                RecordType::A,
            ));
            message.to_vec().unwrap()
        }

        fn new() -> Self {
            Self {
                request: Mutex::new(None),
                response: Self::response(),
            }
        }
    }

    impl DohHttpTransport for FakeDohTransport {
        fn post<'a>(
            &'a self,
            request: DohHttpRequest,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
            *self.request.lock().unwrap() = Some(request);
            let body = self.response.clone();
            Box::pin(async move {
                Ok(DohHttpResponseOwned {
                    status: 200,
                    content_type: Some("application/dns-message".to_owned()),
                    body,
                })
            })
        }
    }

    #[test]
    fn upstream_runtime_registers_plain_http_doh_through_registry() {
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("remote").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: Some("192.0.2.44".parse().unwrap()),
            proxy: None,
            edns_client_subnet: Some(ResolvedEcs {
                mode: EcsMode::Disabled,
                custom_ip: None,
                source: ValueSource::Upstream,
            }),
        };

        let registry = UpstreamRegistry::from_resolved(std::slice::from_ref(&doh)).unwrap();
        let runtime = UpstreamRuntime::from_registry(&[doh], registry).unwrap();
        let connector = runtime
            .direct
            .get(&ConfigId::new("remote").unwrap())
            .expect("plain HTTP DoH connector must be registered");
        assert_eq!(connector.connector_id().as_str(), "remote");
        assert_eq!(runtime.len(), 1);
    }

    #[test]
    fn upstream_runtime_propagates_unsupported_doh_features() {
        let error = PolicyDnsCore::from_config(
            doh_config_with_address("https://dns.example.test/dns-query").as_ref(),
            42,
        )
        .unwrap_err();
        let super::PolicyCoreBuildError::Upstream { upstream, reason } = error else {
            panic!("expected upstream build error");
        };
        assert_eq!(upstream, "remote");
        assert!(reason.contains("doh_https"));
    }

    #[tokio::test]
    async fn policy_core_uses_injected_registry_for_doh_exchange() {
        let config = doh_config();
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();

        let outcome = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(response) = outcome else {
            panic!("expected injected DoH response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::NoData);

        let request = transport.request.lock().unwrap().clone().unwrap();
        assert_eq!(
            request.endpoint().as_str(),
            "http://dns.example.test/dns-query"
        );
        assert_eq!(request.connect_ip(), Some("192.0.2.44".parse().unwrap()));
        assert_eq!(Message::from_vec(request.body()).unwrap().id, 1);
    }

    fn config() -> std::sync::Arc<crate::config::ResolvedConfig> {
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(
                r#"
version: 1
work:
  path: /tmp/fluxdns-policy-core-test
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5300
    strategy: default
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "127.0.0.1 upstream.example"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: "192.0.2.10 local.example"
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
"#,
            )
            .expect("policy core fixture must be valid")
            .resolved
    }

    fn doh_config() -> std::sync::Arc<crate::config::ResolvedConfig> {
        doh_config_with_address("http://dns.example.test/dns-query")
    }

    fn doh_config_with_address(address: &str) -> std::sync::Arc<crate::config::ResolvedConfig> {
        let source = r#"
version: 1
work:
  path: /tmp/fluxdns-policy-doh-test
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5302
    strategy: default
upstreams:
  - type: doh
    name: remote
    address: __DOH_ADDRESS__
    connect_ip: 192.0.2.44
strategy:
  - name: default
    rules:
      - hosts: unused-hosts
    default_upstream: remote
hosts:
  - type: const
    name: unused-hosts
    format: hosts
    hosts: "192.0.2.99 unused.example"
        "#
        .replace("__DOH_ADDRESS__", address);
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("policy DoH fixture must be valid")
            .resolved
    }

    #[test]
    fn policy_core_accepts_configured_plain_http_doh_with_disabled_ecs() {
        let core = PolicyDnsCore::from_config(doh_config().as_ref(), 42).unwrap();
        assert_eq!(core.upstream_count(), 1);
    }

    #[tokio::test]
    async fn policy_core_from_config_executes_proxy_doh_upstream() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();

            let mut connect = [0_u8; 10];
            stream.read_exact(&mut connect).await.unwrap();
            assert_eq!(connect, [5, 1, 0, 1, 192, 0, 2, 44, 0, 80]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();

            let mut bytes = Vec::new();
            let header_end;
            let body_end;
            loop {
                let mut chunk = [0_u8; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let header_end_candidate = end + 4;
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                let content_length = headers
                    .split("\r\n")
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= header_end_candidate + content_length {
                    header_end = header_end_candidate;
                    body_end = header_end_candidate + content_length;
                    assert!(headers.starts_with("POST /dns-query HTTP/1.1\r\n"));
                    assert!(headers.contains("Host: dns.example.test\r\n"));
                    break;
                }
            }

            let request = Message::from_vec(&bytes[header_end..body_end]).unwrap();
            let mut response =
                Message::new(request.metadata.id, MessageType::Response, OpCode::Query);
            response.metadata.response_code = ResponseCode::NoError;
            response.add_query(request.queries[0].clone());
            let response_body = response.to_vec().unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&response_body).await.unwrap();
        });

        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "fluxdns-policy-proxy-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let secret_path = root.join("proxy-url");
        fs::write(&secret_path, format!("socks5://127.0.0.1:{proxy_port}")).unwrap();

        let mut config = Arc::try_unwrap(doh_config()).ok().unwrap();
        config.outbounds.push(ResolvedOutbound {
            id: ConfigId::new("socks").unwrap(),
            kind: crate::config::model::OutboundType::Socks5,
            proxy_url: ResolvedSecretRef {
                env: None,
                file: Some(secret_path),
            },
        });
        let ResolvedUpstream::Doh { proxy, .. } = &mut config.upstreams[0] else {
            panic!("expected DoH upstream");
        };
        *proxy = Some(ConfigId::new("socks").unwrap());

        let core = PolicyDnsCore::from_config(&config, 42).unwrap();
        let CoreOutcome::Response(response) = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected proxied upstream response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::NoData);
        server.await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn group_config() -> std::sync::Arc<crate::config::ResolvedConfig> {
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(
                r#"
version: 1
work:
  path: /tmp/fluxdns-policy-group-test
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5301
    strategy: default
upstreams:
  - type: hosts
    name: first
    format: hosts
    hosts: "192.0.2.11 group.example"
  - type: hosts
    name: second
    format: hosts
    hosts: "192.0.2.12 group.example"
  - type: group
    name: group
    upstreams:
      - name: first
        weight: 1
      - name: second
        weight: 1
    upstream_mode: round-robin
    timeout: 1s
hosts:
  - type: const
    name: unused-hosts
    format: hosts
    hosts: "192.0.2.99 unused.example"
strategy:
  - name: default
    rules:
      - hosts: unused-hosts
    default_upstream: group
"#,
            )
            .expect("policy group fixture must be valid")
            .resolved
    }

    fn request(name: &str, record_type: RecordType) -> DnsRequest {
        let mut message = Message::new(7, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(Name::from_str(name).unwrap(), record_type));
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
                    listener_id: ListenerId::from("dns"),
                    route_id: None,
                    original_dns_id: Some(7),
                },
                client: crate::dns::ClientIdentity {
                    peer_addr: Some(SocketAddr::from(([127, 0, 0, 1], 5300))),
                    client_addr: Some(IpAddr::from([127, 0, 0, 1])),
                    client_id: None,
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
    async fn policy_hosts_rule_produces_local_answer_and_nodata() {
        let core = PolicyDnsCore::from_config(config().as_ref(), 42).unwrap();
        assert_eq!(core.host_resource_count(), 1);
        assert_eq!(core.upstream_count(), 1);

        let answer = core
            .resolve(&request("local.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(answer) = answer else {
            panic!("expected local response");
        };
        assert_eq!(answer.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(answer.ttl().min_ttl, Some(42));

        let nodata = core
            .resolve(&request("local.example.", RecordType::AAAA))
            .await
            .unwrap();
        let CoreOutcome::Response(nodata) = nodata else {
            panic!("expected nodata response");
        };
        assert_eq!(nodata.class(), crate::dns::ResponseClass::NoData);
    }

    #[tokio::test]
    async fn policy_without_local_match_uses_supported_upstream() {
        let core = PolicyDnsCore::from_config(config().as_ref(), 42).unwrap();
        let response = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(response) = response else {
            panic!("expected upstream response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::NxDomain);
    }

    #[tokio::test]
    async fn policy_executes_group_with_supported_hosts_members() {
        let core = PolicyDnsCore::from_config(group_config().as_ref(), 42).unwrap();
        assert_eq!(core.host_resource_count(), 1);
        assert_eq!(core.upstream_count(), 3);

        let response = core
            .resolve(&request("group.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(response) = response else {
            panic!("expected group response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(response.ttl().min_ttl, Some(crate::dns::DEFAULT_LOCAL_TTL));
    }

    #[tokio::test]
    async fn policy_executes_fallback_after_primary_servfail() {
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(
                r#"
version: 1
work:
  path: /tmp/fluxdns-policy-fallback-test
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5302
    strategy: default
upstreams:
  - type: doh
    name: primary
    address: http://dns.example.test/dns-query
    connect_ip: 192.0.2.44
  - type: hosts
    name: fallback
    format: hosts
    hosts: "192.0.2.12 fallback.example"
  - type: group
    name: group
    upstreams:
      - name: primary
        weight: 1
    upstream_mode: failover
    timeout: 1s
    fallbacks:
      - name: fallback
        weight: 1
    fallback_upstream_mode: failover
    fallback_timeout: 1s
hosts:
  - type: const
    name: unused-hosts
    format: hosts
    hosts: "192.0.2.99 unused.example"
strategy:
  - name: default
    rules:
      - hosts: unused-hosts
    default_upstream: group
"#,
            )
            .expect("policy fallback fixture must be valid")
            .resolved;
        let direct = super::direct_upstreams(&config.upstreams);
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &direct,
            Arc::new(ServFailDohTransport),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();

        let CoreOutcome::Response(response) = core
            .resolve(&request("fallback.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected fallback response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(response.ttl().min_ttl, Some(crate::dns::DEFAULT_LOCAL_TTL));
    }
}
