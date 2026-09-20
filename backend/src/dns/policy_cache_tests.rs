use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::rdata::opt::{EdnsCode, EdnsOption};
use hickory_proto::rr::rdata::{A, HTTPS, SVCB};
use hickory_proto::rr::{Name, RData, Record, RecordType};

use crate::cache::{CacheCommitOutcome, CacheLookup};
use crate::config::resolve::ResolvedUpstream;
use crate::config::{ConfigV2Loader, LoadOptions};
use crate::dns::{
    CacheCompatibilityKey, Cancellation, CanonicalQuery, CanonicalResponse, ClientId, CoreOutcome,
    Deadline, DnsCore, DnsRequest, ListenerId, RequestContext, RequestId, RequestMeta,
    RuntimeRevision, TransportCapabilities, TransportClass,
};
use crate::ports::cache::{CacheCondition, CacheWriteOutcome};
use crate::ports::exchange::{ConnectorId, UpstreamOutcome};
use crate::ports::storage::StatsSource;
use crate::ports::telemetry::CacheStatus;
use crate::ports::{PortError, PortFuture};
use crate::resource::CanonicalDomain;
use crate::upstream::{
    DohHttpRequest, DohHttpResponseOwned, DohHttpTransport, UpstreamAttempt, UpstreamRegistry,
};

use super::*;

const GLOBAL_ECS: &str = "203.0.113.0/24";
const MEMBER_ECS: &str = "198.51.100.0/24";

#[derive(Clone, Debug)]
struct WireObservation {
    query: Message,
}

struct RecordingDohTransport {
    calls: AtomicUsize,
    wires: Mutex<Vec<WireObservation>>,
    answer_octet: u8,
}

impl RecordingDohTransport {
    fn new(answer_octet: u8) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            wires: Mutex::new(Vec::new()),
            answer_octet,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn wires(&self) -> Vec<WireObservation> {
        self.wires.lock().unwrap().clone()
    }
}

impl DohHttpTransport for RecordingDohTransport {
    fn post<'a>(
        &'a self,
        request: DohHttpRequest,
        _deadline: Deadline,
        _cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let message = Message::from_vec(request.body()).expect("recorded DoH request must be DNS");
        self.wires.lock().unwrap().push(WireObservation {
            query: message.clone(),
        });
        let query = CanonicalQuery::from_message(message.clone()).unwrap();
        let data = match query.question().query_type() {
            RecordType::A => RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, self.answer_octet))),
            RecordType::HTTPS => RData::HTTPS(HTTPS(SVCB::new(1, Name::root(), Vec::new()))),
            other => panic!("unsupported regression-test qtype: {other:?}"),
        };
        let mut response = CanonicalResponse::response_with_answers(
            &query,
            [Record::from_rdata(
                query.question().name().clone(),
                30,
                data,
            )],
        )
        .unwrap()
        .as_message()
        .clone();
        response.metadata.id = message.metadata.id;
        let body = response.to_vec().unwrap();
        Box::pin(async move {
            Ok(DohHttpResponseOwned {
                status: 200,
                content_type: Some("application/dns-message".to_owned()),
                body,
            })
        })
    }
}

#[derive(Clone, Copy)]
enum EcsSpec<'a> {
    Inherit,
    Disabled,
    Client,
    Custom(&'a str),
}

impl EcsSpec<'_> {
    fn yaml(self, indent: &str) -> String {
        match self {
            Self::Inherit => String::new(),
            Self::Disabled => format!("{indent}edns_client_subnet:\n{indent}  mode: disabled\n"),
            Self::Client => format!("{indent}edns_client_subnet:\n{indent}  mode: client\n"),
            Self::Custom(cidr) => format!(
                "{indent}edns_client_subnet:\n{indent}  mode: custom\n{indent}  custom_ip: {cidr}\n"
            ),
        }
    }
}

struct Fixture<'a> {
    name: &'a str,
    global_ecs: EcsSpec<'a>,
    upstreams: String,
    default_upstream: &'a str,
    strategy_ecs: EcsSpec<'a>,
    rules: String,
    rule_sets: String,
    clients: String,
}

fn doh(name: &str, host: &str, ecs: EcsSpec<'_>) -> String {
    format!(
        "  - type: doh\n    name: {name}\n    address: http://{host}/dns-query\n    connect_ip: 192.0.2.44\n{}",
        ecs.yaml("    ")
    )
}

fn group(name: &str, upstreams: &[&str], fallbacks: &[&str], mode: &str) -> String {
    let members = upstreams
        .iter()
        .map(|name| format!("      - name: {name}\n        weight: 1\n"))
        .collect::<String>();
    let fallback = if fallbacks.is_empty() {
        String::new()
    } else {
        let members = fallbacks
            .iter()
            .map(|name| format!("      - name: {name}\n        weight: 1\n"))
            .collect::<String>();
        format!(
            "    fallbacks:\n{members}    fallback_upstream_mode: failover\n    fallback_timeout: 1s\n"
        )
    };
    format!(
        "  - type: group\n    name: {name}\n    upstreams:\n{members}    upstream_mode: {mode}\n    timeout: 1s\n{fallback}"
    )
}

fn load_fixture(fixture: Fixture<'_>) -> Arc<crate::config::ResolvedConfig> {
    let work_path = crate::config::test_support::absolute_path(fixture.name);
    let global_ecs = fixture.global_ecs.yaml("  ");
    let strategy_ecs = fixture.strategy_ecs.yaml("    ");
    let rules = if fixture.rules.is_empty() {
        "      - hosts: unused-hosts\n".to_owned()
    } else {
        fixture.rules
    };
    let rule_sets = if fixture.rule_sets.is_empty() {
        "rule_set: []\n".to_owned()
    } else {
        format!("rule_set:\n{}", fixture.rule_sets)
    };
    let clients = if fixture.clients.is_empty() {
        "clients: []\n".to_owned()
    } else {
        format!("clients:\n{}", fixture.clients)
    };
    let source = format!(
        r#"version: 2
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
  records_path: ./queries
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns:
  cache:
    enabled: true
    memory:
      max_size_bytes: 67108864
    failure_ttl: 5s
    optimistic:
      enabled: true
      answer_ttl: 7s
      max_age: 1h
{global_ecs}listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5302
    strategy: default
upstreams:
{upstreams}hosts:
  - type: const
    name: unused-hosts
    format: hosts
    hosts: "192.0.2.99 unused.example"
{rule_sets}strategy:
  - name: default
    rules:
{rules}    default_upstream: {default_upstream}
{strategy_ecs}{clients}"#,
        upstreams = fixture.upstreams,
        default_upstream = fixture.default_upstream,
    );
    ConfigV2Loader::new(LoadOptions::default().without_snapshot())
        .load_str(&source)
        .unwrap_or_else(|error| panic!("{} fixture must load: {error:?}", fixture.name))
        .resolved
}

fn direct_upstreams(config: &crate::config::ResolvedConfig) -> Vec<ResolvedUpstream> {
    config
        .upstreams
        .iter()
        .filter(|upstream| {
            matches!(
                upstream,
                ResolvedUpstream::Doh { .. } | ResolvedUpstream::Hosts { .. }
            )
        })
        .cloned()
        .collect()
}

fn core_with_transport(
    config: &crate::config::ResolvedConfig,
    transport: Arc<RecordingDohTransport>,
) -> Arc<PolicyDnsCore> {
    let registry =
        UpstreamRegistry::from_resolved_with_doh_transport(&direct_upstreams(config), transport)
            .unwrap();
    Arc::new(PolicyDnsCore::from_config_with_registry(config, 42, registry).unwrap())
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
                peer_addr: Some(SocketAddr::from(([192, 0, 2, 10], 5300))),
                client_addr: Some(IpAddr::from([192, 0, 2, 10])),
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

fn request_with_ecs(name: &str, subnet: &str) -> DnsRequest {
    let mut request = request(name, RecordType::A);
    let network: ipnet::IpNet = subnet.parse().unwrap();
    let prefix = network.prefix_len();
    request.query = request.query.with_edns_client_subnet(Some(
        hickory_proto::rr::rdata::opt::ClientSubnet::new(network.addr(), prefix, 0),
    ));
    request
}

fn plan_for(core: &PolicyDnsCore, request: &DnsRequest) -> crate::policy::ResolutionPlan {
    let qname = CanonicalDomain::parse(&request.query.question().name().to_ascii()).unwrap();
    core.policy()
        .evaluate(crate::policy::PolicyRequest {
            listener_id: &ConfigId::new("dns").unwrap(),
            doh_route_id: None,
            client_id: request
                .context
                .client
                .client_id
                .as_ref()
                .map(|value| value.as_str()),
            client_addr: request.context.client.client_addr,
            client_digest: None,
            qname: Some(&qname),
        })
        .unwrap()
}

async fn resolve_and_commit(
    core: &PolicyDnsCore,
    request: &DnsRequest,
) -> crate::dns::DnsResolutionObservation {
    let mut completion = core.resolve_with_completion(request).await;
    let CoreOutcome::Response(response) = completion.result.unwrap() else {
        panic!("expected DNS response");
    };
    assert!(
        response
            .ttl()
            .min_ttl
            .is_some_and(|ttl| ttl > 0 && ttl <= 30)
    );
    if let Some(candidate) = completion.cache_commit.take() {
        assert_eq!(
            candidate.commit(Duration::from_secs(1)).await,
            CacheCommitOutcome::Stored
        );
    }
    completion
        .observation
        .expect("policy observation is required")
}

fn assert_upstream(observation: &crate::dns::DnsResolutionObservation, status: CacheStatus) {
    assert_eq!(observation.source, StatsSource::Upstream);
    assert_eq!(observation.cache_status, status);
}

fn assert_fresh(observation: &crate::dns::DnsResolutionObservation) {
    assert_eq!(observation.source, StatsSource::Cache);
    assert_eq!(observation.cache_status, CacheStatus::Fresh);
}

fn wire_ecs(message: &Message) -> Option<(IpAddr, u8)> {
    match message
        .edns
        .as_ref()
        .and_then(|edns| edns.option(EdnsCode::Subnet))
    {
        Some(EdnsOption::Subnet(subnet)) => Some((subnet.addr(), subnet.source_prefix())),
        _ => None,
    }
}

async fn cache_record(
    core: &PolicyDnsCore,
    key: &crate::ports::cache::CacheKey,
) -> crate::ports::cache::CacheRecord {
    match core
        .cache()
        .lookup(key, Deadline::new(Instant::now() + Duration::from_secs(1)))
        .await
        .unwrap()
    {
        CacheLookup::Fresh(record) | CacheLookup::Stale { record, .. } => record,
        other => panic!("expected cache record, got {other:?}"),
    }
}

async fn force_stale(
    core: &PolicyDnsCore,
    key: &crate::ports::cache::CacheKey,
) -> crate::ports::cache::CacheRecord {
    let record = cache_record(core, key).await;
    let now = Instant::now();
    let stale = Arc::new(crate::ports::cache::CacheEntry {
        response: Arc::clone(&record.entry.response),
        upstream: record.entry.upstream.clone(),
        inserted_at: now - Duration::from_secs(31),
        expires_at: now - Duration::from_millis(1),
        stale_until: Some(now + Duration::from_secs(30)),
        response_class: record.entry.response_class,
        producer_revision: record.entry.producer_revision,
        quality: record.entry.quality,
        checksum: record.entry.checksum,
        format_version: record.entry.format_version,
    });
    let version = match core
        .cache()
        .store()
        .compare_and_swap(
            key.clone(),
            CacheCondition::Version(record.version),
            stale,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap()
    {
        CacheWriteOutcome::Replaced(version) => version,
        other => panic!("forcing stale entry must replace the record: {other:?}"),
    };
    let updated = cache_record(core, key).await;
    assert_eq!(updated.version, version);
    updated
}

fn publish_current(core: &Arc<PolicyDnsCore>, revision: u64) -> Arc<RuntimeCoreCell> {
    let cell = Arc::new(RuntimeCoreCell::default());
    core.attach_runtime_cell(Arc::clone(&cell));
    cell.publish(Some(Arc::new(RuntimeCoreTarget {
        core: Arc::clone(core),
        revision: RuntimeRevision(revision),
    })));
    cell
}

async fn drain(core: &PolicyDnsCore) {
    tokio::time::timeout(
        Duration::from_secs(5),
        core.finalizer_owner().wait_idle_for_test(),
    )
    .await
    .expect("finalizer drain must stay bounded");
}

fn positive_response(request: &DnsRequest, octet: u8) -> CanonicalResponse {
    CanonicalResponse::response_with_answers(
        &request.query,
        [Record::from_rdata(
            request.query.question().name().clone(),
            30,
            RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, octet))),
        )],
    )
    .unwrap()
}

fn equivalent_group_fixture(
    name: &'static str,
    global: &str,
    member: &str,
) -> Arc<crate::config::ResolvedConfig> {
    let fallback_ecs = if global == member {
        EcsSpec::Inherit
    } else {
        EcsSpec::Custom(member)
    };
    let upstreams = [
        doh("primary", "primary.example.test", EcsSpec::Custom(member)),
        doh("inherited", "inherited.example.test", fallback_ecs),
        group("inner-a", &["primary"], &[], "failover"),
        group("inner-b", &["primary"], &[], "failover"),
        group("group", &["inner-a", "inner-b"], &["inherited"], "failover"),
    ]
    .concat();
    load_fixture(Fixture {
        name,
        global_ecs: EcsSpec::Custom(global),
        upstreams,
        default_upstream: "group",
        strategy_ecs: EcsSpec::Inherit,
        rules: String::new(),
        rule_sets: String::new(),
        clients: String::new(),
    })
}

#[tokio::test]
async fn equivalent_group_ecs_is_cached_for_a_and_https_through_nested_duplicate_members() {
    let config = equivalent_group_fixture("policy-cache-equivalent", GLOBAL_ECS, GLOBAL_ECS);
    let transport = Arc::new(RecordingDohTransport::new(11));
    let core = core_with_transport(&config, Arc::clone(&transport));

    for (index, qtype) in [RecordType::A, RecordType::HTTPS].into_iter().enumerate() {
        let request = request("equivalent.example.", qtype);
        let first = resolve_and_commit(&core, &request).await;
        assert_upstream(&first, CacheStatus::Miss);
        let second = resolve_and_commit(&core, &request).await;
        assert_fresh(&second);
        assert_eq!(transport.calls(), index + 1);
        let CoreOutcome::Response(response) = core.resolve(&request).await.unwrap() else {
            panic!("cached answer must remain a response");
        };
        assert!(matches!(response.ttl().min_ttl, Some(29 | 30)));
    }

    assert_eq!(transport.calls(), 2);
    assert!(
        transport
            .wires()
            .iter()
            .all(|wire| wire_ecs(&wire.query) == Some((IpAddr::from([203, 0, 113, 0]), 24)))
    );
}

#[tokio::test]
async fn uniform_member_ecs_different_from_global_drives_wire_and_cache_key() {
    let config = equivalent_group_fixture("policy-cache-member-key", GLOBAL_ECS, MEMBER_ECS);
    let transport = Arc::new(RecordingDohTransport::new(12));
    let core = core_with_transport(&config, Arc::clone(&transport));
    let request = request("member-key.example.", RecordType::A);
    let plan = plan_for(&core, &request);
    let key = cache_key(&core, &plan, &request).expect("uniform member query must have a key");

    assert_upstream(
        &resolve_and_commit(&core, &request).await,
        CacheStatus::Miss,
    );
    assert_fresh(&resolve_and_commit(&core, &request).await);
    assert!(matches!(
        core.cache()
            .lookup(&key, request.context.meta.deadline)
            .await
            .unwrap(),
        CacheLookup::Fresh(_)
    ));
    assert_eq!(transport.calls(), 1);
    assert_eq!(
        wire_ecs(&transport.wires()[0].query),
        Some((IpAddr::from([198, 51, 100, 0]), 24))
    );
}

#[tokio::test]
async fn heterogeneous_fallback_ecs_bypasses_while_normalized_client_equivalence_can_cache() {
    for (label, fallback) in [
        ("custom", EcsSpec::Custom("192.0.2.0/24")),
        ("disabled", EcsSpec::Disabled),
    ] {
        let upstreams = [
            doh(
                "primary",
                "primary.example.test",
                EcsSpec::Custom(MEMBER_ECS),
            ),
            doh("fallback", "fallback.example.test", fallback),
            group("group", &["primary"], &["fallback"], "failover"),
        ]
        .concat();
        let config = load_fixture(Fixture {
            name: if label == "custom" {
                "policy-cache-fallback-custom"
            } else {
                "policy-cache-fallback-disabled"
            },
            global_ecs: EcsSpec::Custom(GLOBAL_ECS),
            upstreams,
            default_upstream: "group",
            strategy_ecs: EcsSpec::Inherit,
            rules: String::new(),
            rule_sets: String::new(),
            clients: String::new(),
        });
        let transport = Arc::new(RecordingDohTransport::new(13));
        let core = core_with_transport(&config, Arc::clone(&transport));
        let request = request("heterogeneous.example.", RecordType::A);
        for _ in 0..2 {
            assert_upstream(
                &resolve_and_commit(&core, &request).await,
                CacheStatus::Disabled,
            );
        }
        assert_eq!(transport.calls(), 2, "fallback mode {label}");
    }

    for (label, request_ecs, cached) in [
        ("same", Some("198.51.100.42/24"), true),
        ("different", Some("203.0.113.42/24"), false),
        ("absent", None, false),
    ] {
        let upstreams = [
            doh(
                "primary",
                "primary.example.test",
                EcsSpec::Custom(MEMBER_ECS),
            ),
            doh("fallback", "fallback.example.test", EcsSpec::Client),
            group("group", &["primary"], &["fallback"], "failover"),
        ]
        .concat();
        let config = load_fixture(Fixture {
            name: match label {
                "same" => "policy-cache-client-equivalent",
                "different" => "policy-cache-client-different",
                _ => "policy-cache-client-absent",
            },
            global_ecs: EcsSpec::Custom(GLOBAL_ECS),
            upstreams,
            default_upstream: "group",
            strategy_ecs: EcsSpec::Inherit,
            rules: String::new(),
            rule_sets: String::new(),
            clients: String::new(),
        });
        let transport = Arc::new(RecordingDohTransport::new(14));
        let core = core_with_transport(&config, Arc::clone(&transport));
        let request = request_ecs.map_or_else(
            || request("client-fallback.example.", RecordType::A),
            |ecs| request_with_ecs("client-fallback.example.", ecs),
        );
        let first = resolve_and_commit(&core, &request).await;
        let second = resolve_and_commit(&core, &request).await;
        if cached {
            assert_upstream(&first, CacheStatus::Miss);
            assert_fresh(&second);
            assert_eq!(transport.calls(), 1, "client case {label}");
        } else {
            assert_upstream(&first, CacheStatus::Disabled);
            assert_upstream(&second, CacheStatus::Disabled);
            assert_eq!(transport.calls(), 2, "client case {label}");
        }
    }
}

#[tokio::test]
async fn rule_strategy_and_client_ecs_precedence_remains_stable() {
    let upstreams = [
        doh(
            "primary",
            "primary.example.test",
            EcsSpec::Custom("192.0.2.0/24"),
        ),
        group("group", &["primary"], &[], "failover"),
    ]
    .concat();
    let rules = "      - rule_set: routed\n        upstream: group\n        edns_client_subnet:\n          mode: custom\n          custom_ip: 198.18.0.0/15\n      - hosts: unused-hosts\n".to_owned();
    let rule_sets = "  - type: const\n    name: routed\n    format: clash\n    rule: \"DOMAIN-SUFFIX,rule.example\\n\"\n".to_owned();
    let clients = "  - name: client\n    client_id: client-id\n    match:\n      ips: [192.0.2.0/24]\n    edns_client_subnet:\n      mode: custom\n      custom_ip: 100.64.0.0/10\n".to_owned();
    let config = load_fixture(Fixture {
        name: "policy-cache-precedence",
        global_ecs: EcsSpec::Custom(GLOBAL_ECS),
        upstreams,
        default_upstream: "group",
        strategy_ecs: EcsSpec::Custom("198.19.0.0/16"),
        rules,
        rule_sets,
        clients,
    });
    let transport = Arc::new(RecordingDohTransport::new(15));
    let core = core_with_transport(&config, Arc::clone(&transport));

    let mut rule_request = request("www.rule.example.", RecordType::A);
    rule_request.context.client.client_id = Some(ClientId::from("client-id"));
    resolve_and_commit(&core, &rule_request).await;
    let mut strategy_request = request("strategy.example.", RecordType::A);
    strategy_request.context.client.client_id = Some(ClientId::from("client-id"));
    resolve_and_commit(&core, &strategy_request).await;
    let wires = transport.wires();
    assert_eq!(
        wire_ecs(&wires[0].query),
        Some((IpAddr::from([198, 18, 0, 0]), 15))
    );
    assert_eq!(
        wire_ecs(&wires[1].query),
        Some((IpAddr::from([198, 19, 0, 0]), 16))
    );

    let config = load_fixture(Fixture {
        name: "policy-cache-client-precedence",
        global_ecs: EcsSpec::Custom(GLOBAL_ECS),
        upstreams: [
            doh("primary", "primary.example.test", EcsSpec::Custom("192.0.2.0/24")),
            group("group", &["primary"], &[], "failover"),
        ]
        .concat(),
        default_upstream: "group",
        strategy_ecs: EcsSpec::Inherit,
        rules: String::new(),
        rule_sets: String::new(),
        clients: "  - name: client\n    client_id: client-id\n    match:\n      ips: [192.0.2.0/24]\n    edns_client_subnet:\n      mode: custom\n      custom_ip: 100.64.0.0/10\n".to_owned(),
    });
    let transport = Arc::new(RecordingDohTransport::new(16));
    let core = core_with_transport(&config, Arc::clone(&transport));
    let mut client_request = request("client.example.", RecordType::A);
    client_request.context.client.client_id = Some(ClientId::from("client-id"));
    resolve_and_commit(&core, &client_request).await;
    assert_eq!(
        wire_ecs(&transport.wires()[0].query),
        Some((IpAddr::from([100, 64, 0, 0]), 10))
    );

    let config = load_fixture(Fixture {
        name: "policy-cache-disabled-precedence",
        global_ecs: EcsSpec::Custom(GLOBAL_ECS),
        upstreams: doh(
            "remote",
            "remote.example.test",
            EcsSpec::Custom("192.0.2.0/24"),
        ),
        default_upstream: "remote",
        strategy_ecs: EcsSpec::Disabled,
        rules: String::new(),
        rule_sets: String::new(),
        clients: "  - name: client\n    client_id: client-id\n    match:\n      ips: [192.0.2.0/24]\n    edns_client_subnet:\n      mode: custom\n      custom_ip: 100.64.0.0/10\n".to_owned(),
    });
    let transport = Arc::new(RecordingDohTransport::new(17));
    let core = core_with_transport(&config, Arc::clone(&transport));
    let mut disabled_request = request("disabled.example.", RecordType::A);
    disabled_request.context.client.client_id = Some(ClientId::from("client-id"));
    resolve_and_commit(&core, &disabled_request).await;
    assert_eq!(wire_ecs(&transport.wires()[0].query), None);
}

async fn steady_state_refresh_case(resolved: bool) {
    let config = if resolved {
        equivalent_group_fixture("policy-cache-steady-resolved", GLOBAL_ECS, MEMBER_ECS)
    } else {
        load_fixture(Fixture {
            name: "policy-cache-steady-fast",
            global_ecs: EcsSpec::Disabled,
            upstreams: doh("remote", "remote.example.test", EcsSpec::Inherit),
            default_upstream: "remote",
            strategy_ecs: EcsSpec::Inherit,
            rules: String::new(),
            rule_sets: String::new(),
            clients: String::new(),
        })
    };
    let transport = Arc::new(RecordingDohTransport::new(if resolved { 18 } else { 17 }));
    let core = core_with_transport(&config, Arc::clone(&transport));
    let request = request(
        if resolved {
            "steady-resolved.example."
        } else {
            "steady-fast.example."
        },
        RecordType::A,
    );
    let plan = plan_for(&core, &request);
    let key = cache_key(&core, &plan, &request).unwrap();
    assert_upstream(
        &resolve_and_commit(&core, &request).await,
        CacheStatus::Miss,
    );
    let stale = force_stale(&core, &key).await;
    let stale_expiry = stale.entry.expires_at;
    let _cell = publish_current(&core, 1);

    let observation = resolve_and_commit(&core, &request).await;
    assert_eq!(observation.source, StatsSource::Cache);
    assert_eq!(observation.cache_status, CacheStatus::Stale);
    drain(&core).await;

    let refreshed = cache_record(&core, &key).await;
    assert!(refreshed.version.0 > stale.version.0);
    assert!(refreshed.entry.expires_at > stale_expiry);
    assert!(refreshed.entry.expires_at > Instant::now());
    assert_eq!(transport.calls(), 2);
    assert_fresh(&resolve_and_commit(&core, &request).await);
    assert_eq!(transport.calls(), 2);
}

#[tokio::test]
async fn published_current_core_refreshes_fast_and_resolved_stale_entries() {
    steady_state_refresh_case(false).await;
    steady_state_refresh_case(true).await;
}

async fn cross_runtime_pair(
    name: &'static str,
) -> (
    Arc<PolicyDnsCore>,
    Arc<PolicyDnsCore>,
    Arc<RecordingDohTransport>,
    Arc<RecordingDohTransport>,
    DnsRequest,
    crate::ports::cache::CacheKey,
    crate::ports::cache::CacheKey,
) {
    let old_config = equivalent_group_fixture(name, GLOBAL_ECS, MEMBER_ECS);
    let latest_config = equivalent_group_fixture(
        if name.ends_with("stale") {
            "policy-cache-cross-latest-stale"
        } else {
            "policy-cache-cross-latest-fresh"
        },
        GLOBAL_ECS,
        "198.51.100.128/25",
    );
    let old_transport = Arc::new(RecordingDohTransport::new(20));
    let latest_transport = Arc::new(RecordingDohTransport::new(21));
    let old = core_with_transport(&old_config, Arc::clone(&old_transport));
    let latest = core_with_transport(&latest_config, Arc::clone(&latest_transport));
    let request = request("cross-runtime.example.", RecordType::A);
    let old_key = cache_key(&old, &plan_for(&old, &request), &request).unwrap();
    let latest_key = cache_key(&latest, &plan_for(&latest, &request), &request).unwrap();
    assert_ne!(old_key, latest_key);
    resolve_and_commit(&old, &request).await;
    resolve_and_commit(&latest, &request).await;
    force_stale(&old, &old_key).await;
    let cell = Arc::new(RuntimeCoreCell::default());
    old.attach_runtime_cell(Arc::clone(&cell));
    latest.attach_runtime_cell(Arc::clone(&cell));
    cell.publish(Some(Arc::new(RuntimeCoreTarget {
        core: Arc::clone(&latest),
        revision: RuntimeRevision(2),
    })));
    (
        old,
        latest,
        old_transport,
        latest_transport,
        request,
        old_key,
        latest_key,
    )
}

#[tokio::test]
async fn cross_runtime_refresh_uses_latest_ecs_and_preserves_fresh_target() {
    let (old, latest, old_transport, latest_transport, request, _, latest_key) =
        cross_runtime_pair("policy-cache-cross-fresh").await;
    let before = cache_record(&latest, &latest_key).await;

    let observation = resolve_and_commit(&old, &request).await;
    assert_eq!(observation.source, StatsSource::Cache);
    assert_eq!(observation.cache_status, CacheStatus::Stale);
    drain(&latest).await;

    let after = cache_record(&latest, &latest_key).await;
    assert_eq!(after.version, before.version);
    assert_eq!(old_transport.calls(), 1);
    assert_eq!(latest_transport.calls(), 1);
}

#[tokio::test]
async fn cross_runtime_refresh_safely_replaces_target_stale_with_latest_query() {
    let (old, latest, old_transport, latest_transport, request, _, latest_key) =
        cross_runtime_pair("policy-cache-cross-stale").await;
    let target_stale = force_stale(&latest, &latest_key).await;

    let observation = resolve_and_commit(&old, &request).await;
    assert_eq!(observation.source, StatsSource::Cache);
    assert_eq!(observation.cache_status, CacheStatus::Stale);
    drain(&latest).await;

    let refreshed = cache_record(&latest, &latest_key).await;
    assert!(refreshed.version.0 > target_stale.version.0);
    assert!(refreshed.entry.expires_at > target_stale.entry.expires_at);
    assert_eq!(old_transport.calls(), 1);
    assert_eq!(latest_transport.calls(), 2);
    assert_eq!(
        wire_ecs(&latest_transport.wires().last().unwrap().query),
        Some((IpAddr::from([198, 51, 100, 128]), 25))
    );
}

async fn submit_late(
    old: &PolicyDnsCore,
    request: &DnsRequest,
    key: &crate::ports::cache::CacheKey,
    octet: u8,
) {
    let plan = plan_for(old, request);
    old.late_result_sink(key, request, &plan.upstream).submit(
        request.query.clone(),
        request.context.clone(),
        UpstreamAttempt {
            attempt_index: 1,
            connector: ConnectorId::new("remote").unwrap(),
            outcome: UpstreamOutcome::Response(positive_response(request, octet)),
        },
    );
}

#[tokio::test]
async fn late_result_cross_store_accepts_same_semantics_and_rejects_changed_upstream() {
    let same_config = load_fixture(Fixture {
        name: "policy-cache-late-same",
        global_ecs: EcsSpec::Disabled,
        upstreams: doh("remote", "same.example.test", EcsSpec::Inherit),
        default_upstream: "remote",
        strategy_ecs: EcsSpec::Inherit,
        rules: String::new(),
        rule_sets: String::new(),
        clients: String::new(),
    });
    let old = core_with_transport(&same_config, Arc::new(RecordingDohTransport::new(22)));
    let same_latest = core_with_transport(&same_config, Arc::new(RecordingDohTransport::new(23)));
    let request = request("late-semantics.example.", RecordType::A);
    let old_key = cache_key(&old, &plan_for(&old, &request), &request).unwrap();
    let same_key = cache_key(&same_latest, &plan_for(&same_latest, &request), &request).unwrap();
    assert_eq!(old_key, same_key);
    let cell = Arc::new(RuntimeCoreCell::default());
    old.attach_runtime_cell(Arc::clone(&cell));
    same_latest.attach_runtime_cell(Arc::clone(&cell));
    cell.publish(Some(Arc::new(RuntimeCoreTarget {
        core: Arc::clone(&same_latest),
        revision: RuntimeRevision(2),
    })));
    submit_late(&old, &request, &old_key, 24).await;
    drain(&same_latest).await;
    assert!(matches!(
        same_latest
            .cache()
            .lookup(&same_key, request.context.meta.deadline)
            .await
            .unwrap(),
        CacheLookup::Fresh(_)
    ));

    for (index, endpoint) in [
        "http://changed.example.test/dns-query",
        "http://same.example.test/other/dns-query",
        "http://same.example.test/dns-query?route=other",
    ]
    .into_iter()
    .enumerate()
    {
        // SafeUrl 的 Debug 会隐藏后两个差异，必须使用完整端点摘要隔离缓存。
        let upstreams = doh("remote", "same.example.test", EcsSpec::Inherit)
            .replace("http://same.example.test/dns-query", endpoint);
        let changed_config = load_fixture(Fixture {
            name: "policy-cache-late-changed",
            global_ecs: EcsSpec::Disabled,
            upstreams,
            default_upstream: "remote",
            strategy_ecs: EcsSpec::Inherit,
            rules: String::new(),
            rule_sets: String::new(),
            clients: String::new(),
        });
        let changed_latest =
            core_with_transport(&changed_config, Arc::new(RecordingDohTransport::new(25)));
        let changed_key = cache_key(
            &changed_latest,
            &plan_for(&changed_latest, &request),
            &request,
        )
        .unwrap();
        assert_ne!(old_key, changed_key);
        changed_latest.attach_runtime_cell(Arc::clone(&cell));
        cell.publish(Some(Arc::new(RuntimeCoreTarget {
            core: Arc::clone(&changed_latest),
            revision: RuntimeRevision(3 + index as u64),
        })));
        submit_late(&old, &request, &old_key, 26).await;
        drain(&changed_latest).await;
        for key in [&old_key, &changed_key] {
            assert!(matches!(
                changed_latest
                    .cache()
                    .lookup(key, request.context.meta.deadline)
                    .await
                    .unwrap(),
                CacheLookup::Miss
            ));
        }
    }
}
