//! 答案 key v3 与 scope 的策略/store/快照组合回归。
use super::*;
use crate::cache::{CacheSnapshotOwner, CacheSnapshotSettings, CacheWriteResult};
use crate::dns::DnsMessageId;
use crate::ports::cache::{CacheInvalidation, CacheKey};
use hickory_proto::op::Edns;
use hickory_proto::rr::rdata::opt::ClientSubnet;

#[derive(Clone, Copy)]
enum ReplyEcs {
    Absent,
    Scope(u8),
    Mismatch,
}

struct ScopeTransport {
    inner: RecordingDohTransport,
    reply: Mutex<ReplyEcs>,
}

impl DohHttpTransport for ScopeTransport {
    fn post<'a>(
        &'a self,
        request: DohHttpRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
        let query =
            CanonicalQuery::from_message(Message::from_vec(request.body()).unwrap()).unwrap();
        let reply = *self.reply.lock().unwrap();
        Box::pin(async move {
            let mut result = self.inner.post(request, deadline, cancellation).await?;
            let mut response = Message::from_vec(&result.body).unwrap();
            response.edns = None;
            if let Some(ecs) = reply_subnet(&query, reply) {
                let mut edns = Edns::new();
                edns.options_mut().insert(EdnsOption::Subnet(ecs));
                response.set_edns(edns);
            }
            result.body = response.to_vec().unwrap();
            Ok(result)
        })
    }
}

fn reply_subnet(query: &CanonicalQuery, reply: ReplyEcs) -> Option<ClientSubnet> {
    match reply {
        ReplyEcs::Absent => None,
        ReplyEcs::Scope(scope) => query
            .edns_client_subnet()
            .map(|ecs| ClientSubnet::new(ecs.addr(), ecs.source_prefix(), scope)),
        ReplyEcs::Mismatch => Some(ClientSubnet::new("198.18.0.0".parse().unwrap(), 24, 0)),
    }
}

fn config() -> Arc<crate::config::ResolvedConfig> {
    load_fixture(Fixture {
        name: "policy-scope",
        global_ecs: EcsSpec::Client,
        upstreams: doh("remote", "scope.example.test", EcsSpec::Inherit),
        default_upstream: "remote",
        strategy_ecs: EcsSpec::Inherit,
        rules: String::new(),
        rule_sets: String::new(),
        clients: String::new(),
    })
}

fn scope_core(reply: ReplyEcs) -> (Arc<PolicyDnsCore>, Arc<ScopeTransport>) {
    let config = config();
    let transport = Arc::new(ScopeTransport {
        inner: RecordingDohTransport::new(42),
        reply: Mutex::new(reply),
    });
    let registry = UpstreamRegistry::from_resolved_with_doh_transport(
        &direct_upstreams(&config),
        Arc::clone(&transport),
    )
    .unwrap();
    (
        Arc::new(PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap()),
        transport,
    )
}

fn subnet_request(cidr: Option<&str>) -> DnsRequest {
    match cidr {
        Some(cidr) => request_with_ecs("scope.example.", cidr),
        None => {
            let mut request = request("scope.example.", RecordType::A);
            request.context.client.client_addr = None;
            request
        }
    }
}

async fn seed(core: &PolicyDnsCore, request: &DnsRequest, reply: ReplyEcs, octet: u8) -> CacheKey {
    let prepared = core.prepare_cache_query(request).unwrap();
    let mut message = positive_response(request, octet).as_message().clone();
    message.edns = None;
    if let Some(ecs) = reply_subnet(&prepared.query, reply) {
        let mut edns = Edns::new();
        edns.options_mut().insert(EdnsOption::Subnet(ecs));
        message.set_edns(edns);
    }
    let response =
        CanonicalResponse::from_message(message, &request.query, DnsMessageId::new(0)).unwrap();
    let key = prepared.answer.storage_key(response.edns_client_subnet());
    assert!(matches!(
        core.cache
            .write_response(CacheWriteRequest {
                key: key.clone(),
                condition: CacheCondition::Absent,
                response: Arc::new(response),
                upstream: cache_upstream_provenance("remote", Some("remote")),
                now: Instant::now(),
                producer_revision: RuntimeRevision(1),
                deadline: request.context.meta.deadline,
            })
            .await
            .unwrap(),
        CacheWriteResult::Stored {
            outcome: CacheWriteOutcome::Inserted(_),
            ..
        }
    ));
    key
}

#[tokio::test]
async fn scope_matrix_shares_only_permitted_query_networks() {
    for (source, reply, next, hit) in [
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Scope(0),
            Some("203.0.113.0/24"),
            true,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Scope(16),
            Some("192.0.9.0/24"),
            true,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Scope(24),
            Some("192.0.9.0/24"),
            false,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Scope(32),
            Some("192.0.2.99/32"),
            true,
        ),
        (
            Some("192.0.0.0/20"),
            ReplyEcs::Scope(24),
            Some("192.0.0.0/20"),
            true,
        ),
        (
            Some("192.0.0.0/20"),
            ReplyEcs::Scope(24),
            Some("192.0.2.0/24"),
            false,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Absent,
            Some("192.0.2.99/32"),
            true,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Absent,
            Some("192.0.9.0/24"),
            false,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Mismatch,
            Some("192.0.2.99/32"),
            true,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Mismatch,
            Some("192.0.9.0/24"),
            false,
        ),
        (None, ReplyEcs::Mismatch, None, true),
        (None, ReplyEcs::Absent, Some("192.0.2.0/24"), false),
        (Some("192.0.2.0/24"), ReplyEcs::Scope(0), None, false),
        (
            Some("0.0.0.0/0"),
            ReplyEcs::Scope(0),
            Some("192.0.2.0/24"),
            false,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Scope(0),
            Some("0.0.0.0/0"),
            true,
        ),
        (
            Some("2001:db8:1::/56"),
            ReplyEcs::Scope(32),
            Some("2001:db8:2::/56"),
            true,
        ),
        (
            Some("2001:db8::/48"),
            ReplyEcs::Scope(64),
            Some("2001:db8::/56"),
            false,
        ),
        (
            Some("192.0.2.0/24"),
            ReplyEcs::Scope(0),
            Some("2001:db8::/56"),
            false,
        ),
    ] {
        let (core, transport) = scope_core(reply);
        let first = subnet_request(source);
        let mut completion = core.resolve_with_completion(&first).await;
        let CoreOutcome::Response(response) = completion.result.unwrap() else {
            panic!("response");
        };
        if matches!(reply, ReplyEcs::Mismatch) {
            assert_eq!(
                response.edns_client_subnet().unwrap().addr(),
                "198.18.0.0".parse::<IpAddr>().unwrap()
            );
        }
        assert_eq!(
            completion
                .cache_commit
                .take()
                .unwrap()
                .commit(Duration::from_secs(1))
                .await,
            CacheCommitOutcome::Stored
        );
        let observation = resolve_and_commit(&core, &subnet_request(next)).await;
        assert_eq!(
            observation.cache_status,
            if hit {
                CacheStatus::Fresh
            } else {
                CacheStatus::Miss
            },
            "{source:?} -> {next:?}"
        );
        assert_eq!(transport.inner.calls(), if hit { 1 } else { 2 });
    }
}

#[tokio::test]
async fn longest_prefix_wins_and_refresh_migrates_scope_without_removing_new_versions() {
    let (core, transport) = scope_core(ReplyEcs::Scope(16));
    let request = subnet_request(Some("192.0.2.0/24"));
    let narrow = seed(&core, &request, ReplyEcs::Scope(24), 11).await;
    let other = seed(
        &core,
        &subnet_request(Some("192.0.9.0/24")),
        ReplyEcs::Scope(24),
        12,
    )
    .await;
    let outside = seed(
        &core,
        &subnet_request(Some("192.1.2.0/24")),
        ReplyEcs::Scope(24),
        13,
    )
    .await;
    let wide = seed(&core, &request, ReplyEcs::Scope(0), 14).await;
    let answer = answer_key(&core, &request).unwrap();
    let policy = plan_for(&core, &request).cache;
    assert_eq!(
        core.probe_answer(&answer, &policy, request.context.meta.deadline, false)
            .await
            .fresh
            .unwrap()
            .version,
        cache_record(&core, &narrow).await.version
    );
    let stale = force_stale(&core, &narrow).await;
    let probe = core
        .probe_answer(&answer, &policy, request.context.meta.deadline, false)
        .await;
    assert!(
        probe.fresh.is_none(),
        "较宽 fresh 不能越过仍可返回的更具体 stale"
    );
    assert_eq!(probe.stale[0].0.version, stale.version);
    drop(probe);
    core.cache
        .store()
        .invalidate(
            CacheInvalidation::Exact(wide),
            request.context.meta.deadline,
        )
        .await
        .unwrap();
    let result = resolve_and_commit(&core, &request).await;
    assert_eq!(result.cache_status, CacheStatus::Stale);
    drain(&core).await;
    assert_eq!(transport.inner.calls(), 1);
    for old in [&narrow, &other] {
        assert!(matches!(
            core.cache
                .lookup(old, request.context.meta.deadline)
                .await
                .unwrap(),
            CacheLookup::Miss
        ));
    }
    assert!(matches!(
        core.cache
            .lookup(&outside, request.context.meta.deadline)
            .await
            .unwrap(),
        CacheLookup::Fresh(_)
    ));
    assert_fresh(&resolve_and_commit(&core, &subnet_request(Some("192.0.9.0/24"))).await);

    // 模拟刷新期间更具体的条目被新写入：版本水位只能删除开始刷新前的值。
    let before = core.cache_snapshot_source.snapshot_generation();
    let concurrent = seed(&core, &request, ReplyEcs::Scope(24), 15).await;
    let broad = seed(&core, &request, ReplyEcs::Scope(0), 16).await;
    let version = cache_record(&core, &broad).await.version;
    core.cache_snapshot_source
        .invalidate_covered(&broad, version, before)
        .unwrap();
    assert!(matches!(
        core.cache
            .lookup(&concurrent, request.context.meta.deadline)
            .await
            .unwrap(),
        CacheLookup::Fresh(_)
    ));
    assert_eq!(
        core.cache_snapshot_source.covering_prefixes(&answer),
        vec![0, 24]
    );
}

#[tokio::test]
async fn scope_snapshot_rebuilds_prefix_index_and_deleted_entries_do_not_hit() {
    let (core, _) = scope_core(ReplyEcs::Scope(16));
    let request = subnet_request(Some("192.0.2.0/24"));
    resolve_and_commit(&core, &request).await;
    let root =
        std::path::PathBuf::from(crate::config::test_support::absolute_path("scope-snapshot"));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!(
        "scope-{}-{}.snapshot",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let settings = || {
        CacheSnapshotSettings::new(true, path.clone(), Duration::from_secs(3600), Vec::new())
            .unwrap()
    };
    let deadline = || Deadline::new(Instant::now() + Duration::from_secs(5));
    let owner = CacheSnapshotOwner::start(
        RuntimeRevision(1),
        core.cache_snapshot_source(),
        settings(),
        deadline(),
    )
    .await
    .unwrap();
    assert!(owner.shutdown(deadline()).await.written);
    let (restored, transport) = scope_core(ReplyEcs::Absent);
    let owner = CacheSnapshotOwner::start(
        RuntimeRevision(2),
        restored.cache_snapshot_source(),
        settings(),
        deadline(),
    )
    .await
    .unwrap();
    assert_eq!(owner.status().recovery.loaded, 1);
    let answer = answer_key(&restored, &request).unwrap();
    assert_eq!(
        restored.cache_snapshot_source.covering_prefixes(&answer),
        vec![16]
    );
    assert_fresh(&resolve_and_commit(&restored, &subnet_request(Some("192.0.9.0/24"))).await);
    assert_eq!(transport.inner.calls(), 0);
    let stale_index = restored.cache_snapshot_source.covering_prefixes(&answer);
    restored
        .cache
        .store()
        .invalidate(CacheInvalidation::All, deadline())
        .await
        .unwrap();
    assert!(
        restored
            .cache_snapshot_source
            .covering_prefixes(&answer)
            .is_empty()
    );
    for key in answer.probe_keys(&stale_index) {
        assert!(matches!(
            restored.cache.lookup(&key, deadline()).await.unwrap(),
            CacheLookup::Miss
        ));
    }
    assert!(owner.shutdown(deadline()).await.completed);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn client_ecs_privacy_caps_clear_host_bits_and_query_scope() {
    let (core, transport) = scope_core(ReplyEcs::Absent);
    for (cidr, prefix) in [
        ("192.0.2.255/32", 24),
        ("192.0.2.255/20", 20),
        ("2001:db8:1:2::ffff/128", 56),
        ("2001:db8:1:2::ffff/48", 48),
    ] {
        let mut request = subnet_request(Some(cidr));
        let ecs = request.query.edns_client_subnet().unwrap();
        request.query = request
            .query
            .with_edns_client_subnet(Some(ClientSubnet::new(ecs.addr(), ecs.source_prefix(), 8)));
        resolve_and_commit(&core, &request).await;
        let wire = transport.inner.wires().last().unwrap().query.clone();
        let ecs = CanonicalQuery::from_message(wire)
            .unwrap()
            .edns_client_subnet()
            .unwrap();
        assert_eq!(ecs.source_prefix(), prefix);
        assert_eq!(ecs.scope_prefix(), 0);
        assert_eq!(
            ecs.addr(),
            ipnet::IpNet::new(ecs.addr(), prefix).unwrap().network()
        );
    }
}

#[test]
#[ignore = "显式运行 release scope 探测基准，不作时延门禁"]
fn scope_probe_benchmark() {
    let (core, _) = scope_core(ReplyEcs::Scope(0));
    for cidr in ["192.0.2.0/24", "2001:db8::/56"] {
        let answer = answer_key(&core, &subnet_request(Some(cidr))).unwrap();
        let max = if cidr.contains(':') { 56 } else { 24 };
        for prefixes in [vec![0, max], (0..=max).collect()] {
            let start = Instant::now();
            for _ in 0..100_000 {
                std::hint::black_box(answer.probe_keys(std::hint::black_box(&prefixes)));
            }
            eprintln!(
                "scope probe encode: family={}, prefixes={}, iterations=100000, elapsed={:?}",
                if max == 24 { 4 } else { 6 },
                prefixes.len(),
                start.elapsed()
            );
        }
    }
}

#[tokio::test]
async fn entry_is_shared_across_listeners_routes_clients_strategies_and_resource_updates() {
    use crate::config::resolve::{ResolvedDohRoute, ResolvedListener};
    use crate::resource::{
        HostsIndex, ResourceSnapshot, ResourceSourceKind, ResourceStaleStatus, RuleIndex,
    };
    let mut config = Arc::try_unwrap(load_fixture(Fixture {
        name: "cache-sharing",
        global_ecs: EcsSpec::Custom(GLOBAL_ECS),
        upstreams: doh("remote", "scope.example.test", EcsSpec::Inherit),
        default_upstream: "remote", strategy_ecs: EcsSpec::Inherit,
        rules: "      - rule_set: routed\n        upstream: remote\n      - hosts: unused-hosts\n".to_owned(),
        rule_sets: "  - type: const\n    name: routed\n    format: clash\n    rule: \"DOMAIN,other.example\\n\"\n".to_owned(),
        clients: "  - name: first\n    client_id: one\n  - name: second\n    client_id: two\n".to_owned(),
    })).unwrap();
    let mut strategy = config.strategies[0].clone();
    strategy.id = ConfigId::new("second").unwrap();
    config.strategies.push(strategy);
    config.listeners.push(ResolvedListener::Tcp {
        id: ConfigId::new("tcp").unwrap(),
        addresses: vec!["127.0.0.1".parse().unwrap()],
        port: 5302,
        strategy: ConfigId::new("second").unwrap(),
        hosts: None,
    });
    config.listeners.push(ResolvedListener::Doh {
        id: ConfigId::new("doh").unwrap(),
        endpoints: Vec::new(),
        routes: ["/dns-query", "/other"]
            .into_iter()
            .map(|path| ResolvedDohRoute {
                path: path.to_owned(),
                strategy: ConfigId::new("second").unwrap(),
                edns_client_subnet: None,
            })
            .collect(),
    });
    let transport = Arc::new(RecordingDohTransport::new(42));
    let core = core_with_transport(&config, Arc::clone(&transport));
    let mut request = subnet_request(Some("192.0.2.0/24"));
    request.context.client.client_id = Some(ClientId::from("one"));
    assert_upstream(
        &resolve_and_commit(&core, &request).await,
        CacheStatus::Miss,
    );
    for (listener, route, class) in [
        ("tcp", None, TransportClass::Stream),
        ("doh", Some("/dns-query"), TransportClass::Multiplexed),
        ("doh", Some("/other"), TransportClass::Multiplexed),
    ] {
        request.context.meta.listener_id = ListenerId::from(listener);
        request.context.meta.route_id = route.map(crate::dns::RouteId::from);
        request.context.transport.class = class;
        request.context.client.client_id = Some(ClientId::from("two"));
        request.context.client.client_addr = Some("198.51.100.55".parse().unwrap());
        assert_fresh(&resolve_and_commit(&core, &request).await);
    }
    assert_eq!(transport.calls(), 1);
    core.publish_rule_set_resource(ResourceSnapshot::new(
        ConfigId::new("routed").unwrap(),
        2,
        1,
        "changed",
        "changed",
        "rule-index-v1",
        SystemTime::UNIX_EPOCH,
        ResourceSourceKind::Remote,
        false,
        ResourceStaleStatus::Fresh,
        RuleIndex::parse(
            "DOMAIN,scope.example\n",
            crate::config::model::RuleSetFormat::Clash,
        )
        .unwrap(),
    ))
    .unwrap();
    assert!(
        resolve_and_commit(&core, &request)
            .await
            .matched_rule
            .is_some()
    );
    assert_fresh(&resolve_and_commit(&core, &request).await);
    core.publish_hosts_resource(ResourceSnapshot::new(
        ConfigId::new("unused-hosts").unwrap(),
        2,
        1,
        "changed",
        "changed",
        "hosts-index-v1",
        SystemTime::UNIX_EPOCH,
        ResourceSourceKind::File,
        false,
        ResourceStaleStatus::Fresh,
        HostsIndex::parse_hosts("192.0.2.2 unrelated.example\n").unwrap(),
    ))
    .unwrap();
    assert_fresh(&resolve_and_commit(&core, &request).await);
    assert_eq!(transport.calls(), 1);
}

#[test]
fn target_content_and_resolved_proxy_changes_invalidate_only_dependent_answers() {
    use crate::config::resolve::{ResolvedOutbound, ResolvedSecretRef};
    let initial = config();
    let first = PolicyDnsCore::from_config(&initial, 42).unwrap();
    let request = subnet_request(Some("192.0.2.0/24"));
    let original = answer_key(&first, &request).unwrap();
    let mut unrelated = Arc::try_unwrap(config()).unwrap();
    let mut other = unrelated.upstreams[0].clone();
    if let ResolvedUpstream::Doh { id, address, .. } = &mut other {
        *id = ConfigId::new("unrelated").unwrap();
        *address = "http://unrelated.example.test/dns-query".parse().unwrap();
    }
    unrelated.upstreams.push(other);
    assert_eq!(
        original,
        answer_key(
            &PolicyDnsCore::from_config(&unrelated, 42).unwrap(),
            &request
        )
        .unwrap()
    );
    if let ResolvedUpstream::Doh { address, .. } = &mut unrelated.upstreams[0] {
        *address = "http://scope.example.test/other?policy=changed"
            .parse()
            .unwrap();
    }
    assert_ne!(
        original,
        answer_key(
            &PolicyDnsCore::from_config(&unrelated, 42).unwrap(),
            &request
        )
        .unwrap()
    );

    let root = std::path::PathBuf::from(crate::config::test_support::absolute_path("scope-proxy"));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("proxy-{}.txt", std::process::id()));
    std::fs::write(&path, "socks5://127.0.0.1:19080").unwrap();
    let mut proxied = Arc::try_unwrap(config()).unwrap();
    proxied.outbounds.push(ResolvedOutbound {
        id: ConfigId::new("proxy").unwrap(),
        kind: crate::config::model::OutboundType::Socks5,
        proxy_url: ResolvedSecretRef {
            env: None,
            file: Some(path.clone()),
            url: None,
        },
    });
    if let ResolvedUpstream::Doh { proxy, .. } = &mut proxied.upstreams[0] {
        *proxy = Some(ConfigId::new("proxy").unwrap());
    }
    let first = PolicyDnsCore::from_config(&proxied, 42).unwrap();
    let before = answer_key(&first, &request).unwrap();
    std::fs::write(&path, "socks5://127.0.0.1:19081").unwrap();
    // 已构造 core 不二次读文件；重建后才采用新代理，与 connector 生命周期一致。
    assert_eq!(before, answer_key(&first, &request).unwrap());
    assert_ne!(
        before,
        answer_key(&PolicyDnsCore::from_config(&proxied, 42).unwrap(), &request).unwrap()
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn covering_index_survives_replacements_and_clears_after_last_removal() {
    let (core, _) = scope_core(ReplyEcs::Scope(24));
    let request = subnet_request(Some("192.0.2.0/24"));
    let answer = answer_key(&core, &request).unwrap();
    let key = seed(&core, &request, ReplyEcs::Scope(24), 42).await;
    for _ in 0..8 {
        let record = cache_record(&core, &key).await;
        assert!(matches!(
            core.cache
                .store()
                .compare_and_swap(
                    key.clone(),
                    CacheCondition::Version(record.version),
                    Arc::clone(&record.entry),
                    request.context.meta.deadline,
                )
                .await
                .unwrap(),
            CacheWriteOutcome::Replaced(_)
        ));
        assert_eq!(
            core.cache_snapshot_source.covering_prefixes(&answer),
            vec![24]
        );
    }
    core.cache
        .store()
        .invalidate(CacheInvalidation::Exact(key), request.context.meta.deadline)
        .await
        .unwrap();
    assert!(
        core.cache_snapshot_source
            .covering_prefixes(&answer)
            .is_empty()
    );
    seed(&core, &request, ReplyEcs::Scope(24), 43).await;
    assert_eq!(
        core.cache_snapshot_source.covering_prefixes(&answer),
        vec![24]
    );
}
