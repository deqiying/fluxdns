//! PolicyIndex 驱动的本地 DNS Core 首轮接线。
//!
//! 该 core 只负责把已编译的 policy/resource 结果转换成本地 hosts 响应。
//! 尚未接入的 upstream/cache 分支统一返回确定性的 SERVFAIL，不伪造网络结果。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use hickory_proto::op::ResponseCode;
use thiserror::Error;

use crate::config::resolve::{ConfigId, ResolvedConfig, ResolvedHostsResource};
use crate::policy::{PolicyBuildError, PolicyIndex, PolicyRequest};
use crate::ports::PortFuture;
use crate::resource::{CanonicalDomain, HostsIndex, HostsLimits, ResourceLoadError, load_hosts};

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
}

/// 使用同一份 resolved config 构建 policy/resource 本地回答 core。
#[derive(Clone, Debug)]
pub struct PolicyDnsCore {
    policy: PolicyIndex,
    hosts: BTreeMap<ConfigId, Arc<HostsIndex>>,
    ttl: u32,
}

impl PolicyDnsCore {
    pub fn from_config(config: &ResolvedConfig, ttl: u32) -> Result<Self, PolicyCoreBuildError> {
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
        Ok(Self { policy, hosts, ttl })
    }

    pub fn policy(&self) -> &PolicyIndex {
        &self.policy
    }

    pub fn host_resource_count(&self) -> usize {
        self.hosts.len()
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

            let Some(resource_id) = plan.hosts else {
                return servfail(request);
            };
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
            response
                .map(CoreOutcome::Response)
                .map_err(CoreError::ResponseConstruction)
        })
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
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr;
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, CoreOutcome, Deadline, DnsCore,
        DnsRequest, ListenerId, RequestContext, RequestId, RequestMeta, RuntimeRevision,
        TransportCapabilities, TransportClass,
    };

    use super::PolicyDnsCore;

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
    async fn policy_without_local_match_returns_deterministic_servfail() {
        let core = PolicyDnsCore::from_config(config().as_ref(), 42).unwrap();
        let response = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(response) = response else {
            panic!("expected servfail response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::ServFail);
    }
}
