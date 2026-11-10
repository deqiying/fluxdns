//! 请求级 ECS 与真实成员 query、缓存候选、刷新生命周期的组合回归。
use super::*;
use crate::dns::ecs::*;
use crate::dns::{RequestTrace, ResponseDelivery};
use hickory_proto::op::Edns;
use hickory_proto::rr::rdata::opt::ClientSubnet;

fn observed_request(mut request: DnsRequest) -> DnsRequest {
    let trace = RequestTrace::new(Instant::now());
    trace.observe_request(&request.query);
    trace.finish_response(ResponseDelivery::Sent, Instant::now());
    request.context.meta.completion = trace;
    request
}

async fn facts(request: &DnsRequest) -> QueryEcs {
    request
        .context
        .meta
        .completion
        .settled_observation()
        .await
        .ecs
        .unwrap()
}

fn fixture(
    global: EcsSpec<'_>,
    upstreams: String,
    target: &str,
) -> Arc<crate::config::ResolvedConfig> {
    load_fixture(Fixture {
        name: "query-ecs-observation",
        global_ecs: global,
        upstreams,
        default_upstream: target,
        strategy_ecs: EcsSpec::Inherit,
        rules: String::new(),
        rule_sets: String::new(),
        clients: String::new(),
    })
}

fn present(cidr: &str, scope_prefix: u8) -> EcsValue {
    EcsValue::Present {
        cidr: cidr.to_owned(),
        scope_prefix,
    }
}

fn exchange(
    exchange: &EcsExchange,
    leaf: &str,
    query: EcsValue,
    source: EcsPolicySource,
    value_source: EcsValueSource,
) {
    let EcsExchange::Observed {
        query: actual,
        policy,
        upstream_leaf_name,
        ..
    } = exchange
    else {
        panic!("expected observed exchange, got {exchange:?}");
    };
    assert_eq!(actual, &query);
    assert_eq!(policy.source, source);
    assert_eq!(policy.value_source, value_source);
    assert_eq!(upstream_leaf_name, leaf);
}

/// 受控 host 失败/延迟只用于测试选择与 late-result，不依赖远端服务。
struct ControlledTransport {
    inner: RecordingDohTransport,
    fail_all: bool,
}
impl DohHttpTransport for ControlledTransport {
    fn post<'a>(
        &'a self,
        request: DohHttpRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
        let host = request.endpoint().host_str().unwrap_or("").to_owned();
        let query = Message::from_vec(request.body()).unwrap();
        let query_ecs = query
            .edns
            .as_ref()
            .and_then(|edns| edns.option(EdnsCode::Subnet))
            .cloned();
        Box::pin(async move {
            if self.fail_all || host.starts_with("fail.") {
                return Err(PortError::new(
                    crate::ports::PortErrorClass::Unavailable,
                    "test_ecs_exchange",
                ));
            }
            if host.starts_with("slow.") {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let mut response = self.inner.post(request, deadline, cancellation).await?;
            let mut message = Message::from_vec(&response.body).unwrap();
            if let Some(EdnsOption::Subnet(subnet)) = query_ecs {
                let mut edns = Edns::new();
                edns.options_mut()
                    .insert(EdnsOption::Subnet(ClientSubnet::new(
                        subnet.addr(),
                        subnet.source_prefix(),
                        0,
                    )));
                message.set_edns(edns);
            }
            response.body = message.to_vec().unwrap();
            Ok(response)
        })
    }
}

fn controlled_core(config: &crate::config::ResolvedConfig, fail_all: bool) -> Arc<PolicyDnsCore> {
    let registry = UpstreamRegistry::from_resolved_with_doh_transport(
        &direct_upstreams(config),
        Arc::new(ControlledTransport {
            inner: RecordingDohTransport::new(44),
            fail_all,
        }),
    )
    .unwrap();
    Arc::new(PolicyDnsCore::from_config_with_registry(config, 42, registry).unwrap())
}

#[tokio::test]
async fn request_and_actual_query_keep_distinct_prefixes_and_scope() {
    for (input, output) in [
        ("203.0.113.99/32", "203.0.113.0/24"),
        ("2001:db8:1234:5678::1/128", "2001:db8:1234:5600::/56"),
        ("203.0.0.0/16", "203.0.0.0/16"),
    ] {
        let config = fixture(
            EcsSpec::Client,
            doh("remote", "ok.example.test", EcsSpec::Inherit),
            "remote",
        );
        let core = controlled_core(&config, false);
        let request = observed_request(request_with_ecs("query-ecs.example.", input));
        resolve_and_commit(&core, &request).await;
        let ecs = facts(&request).await;
        assert_eq!(ecs.request, present(input, 0));
        exchange(
            &ecs.upstream,
            "remote",
            present(output, 0),
            EcsPolicySource::Global,
            EcsValueSource::ClientEcs,
        );
        let EcsExchange::Observed { response, .. } = ecs.upstream else {
            unreachable!()
        };
        assert_eq!(response, present(output, 0));
        assert!(ecs.cache_match.is_none() && ecs.refresh.is_none());
    }
}

#[tokio::test]
async fn client_address_fallback_invalid_input_disabled_and_custom_are_not_conflated() {
    for mode in [
        EcsSpec::Client,
        EcsSpec::Disabled,
        EcsSpec::Custom("2001:db8::/48"),
    ] {
        let config = fixture(
            mode,
            doh("remote", "ok.example.test", EcsSpec::Inherit),
            "remote",
        );
        let core = controlled_core(&config, false);
        let mut input = request("fallback-ecs.example.", RecordType::A);
        input.query = input.query.with_edns_client_subnet(Some(ClientSubnet::new(
            "203.0.113.99".parse().unwrap(),
            33,
            0,
        )));
        let request = observed_request(input);
        resolve_and_commit(&core, &request).await;
        let ecs = facts(&request).await;
        assert_eq!(ecs.request, EcsValue::Invalid);
        let (query, value_source, policy_mode) = match mode {
            EcsSpec::Client => (
                present("192.0.2.0/24", 0),
                EcsValueSource::ClientIp,
                EcsPolicyMode::Client,
            ),
            EcsSpec::Disabled => (
                EcsValue::Absent,
                EcsValueSource::None,
                EcsPolicyMode::Disabled,
            ),
            _ => (
                present("2001:db8::/48", 0),
                EcsValueSource::Custom,
                EcsPolicyMode::Custom,
            ),
        };
        exchange(
            &ecs.upstream,
            "remote",
            query,
            EcsPolicySource::Global,
            value_source,
        );
        let EcsExchange::Observed { policy, .. } = ecs.upstream else {
            unreachable!()
        };
        assert_eq!(policy.mode, policy_mode);
    }
}

#[tokio::test]
async fn all_policy_sources_come_from_the_same_decision() {
    for expected in [
        EcsPolicySource::Default,
        EcsPolicySource::Global,
        EcsPolicySource::Upstream,
        EcsPolicySource::Client,
        EcsPolicySource::Strategy,
        EcsPolicySource::Rule,
    ] {
        let custom = EcsSpec::Custom("198.51.100.0/24");
        let config = load_fixture(Fixture {
            name: "query-ecs-source",
            global_ecs: if expected == EcsPolicySource::Global {
                custom
            } else {
                EcsSpec::Inherit
            },
            upstreams: doh(
                "remote",
                "ok.example.test",
                if expected == EcsPolicySource::Upstream {
                    custom
                } else {
                    EcsSpec::Inherit
                },
            ),
            default_upstream: "remote",
            strategy_ecs: if expected == EcsPolicySource::Strategy {
                custom
            } else {
                EcsSpec::Inherit
            },
            rules: if expected == EcsPolicySource::Rule {
                "      - rule_set: routed\n        upstream: remote\n        edns_client_subnet: {mode: custom, custom_ip: 198.51.100.0/24}\n".into()
            } else {
                String::new()
            },
            rule_sets: if expected == EcsPolicySource::Rule {
                "  - {type: const, name: routed, format: clash, rule: 'DOMAIN-SUFFIX,source.example'}\n".into()
            } else {
                String::new()
            },
            clients: if expected == EcsPolicySource::Client {
                "  - {name: client, client_id: client-id, match: {ips: [192.0.2.0/24]}, edns_client_subnet: {mode: custom, custom_ip: 198.51.100.0/24}}\n".into()
            } else {
                String::new()
            },
        });
        let core = controlled_core(&config, false);
        let request = observed_request(request("source.example.", RecordType::A));
        resolve_and_commit(&core, &request).await;
        let EcsExchange::Observed { policy, .. } = facts(&request).await.upstream else {
            panic!("missing exchange")
        };
        assert_eq!(policy.source, expected);
    }
}

#[tokio::test]
async fn nested_fallback_and_uniform_value_preserve_actual_leaf_policy() {
    for fallback in [false, true] {
        for explicit_first in [false, true] {
            let first = if explicit_first {
                "explicit"
            } else {
                "inherited"
            };
            let second = if explicit_first {
                "inherited"
            } else {
                "explicit"
            };
            let members = if fallback {
                vec!["failed"]
            } else {
                vec![first, second]
            };
            let fallbacks = if fallback {
                vec![first, second]
            } else {
                vec![]
            };
            let upstreams = [
                doh("failed", "fail.example.test", EcsSpec::Disabled),
                doh("explicit", "ok.example.test", EcsSpec::Custom(GLOBAL_ECS)),
                doh("inherited", "ok.example.test", EcsSpec::Inherit),
                group("inner", &members, &fallbacks, "failover"),
                group("outer", &["inner"], &[], "failover"),
            ]
            .concat();
            let config = fixture(EcsSpec::Custom(GLOBAL_ECS), upstreams, "outer");
            let core = controlled_core(&config, false);
            let request = observed_request(request("nested-ecs.example.", RecordType::A));
            let observation = resolve_and_commit(&core, &request).await;
            assert_eq!(observation.upstream_used_id.as_deref(), Some("inner"));
            exchange(
                &facts(&request).await.upstream,
                first,
                present(GLOBAL_ECS, 0),
                if explicit_first {
                    EcsPolicySource::Upstream
                } else {
                    EcsPolicySource::Global
                },
                EcsValueSource::Custom,
            );
        }
    }
}

#[tokio::test]
async fn parallel_late_result_cannot_replace_selected_leaf() {
    let config = fixture(
        EcsSpec::Disabled,
        [
            doh(
                "slow",
                "slow.example.test",
                EcsSpec::Custom("2001:db8::/48"),
            ),
            doh("fast", "ok.example.test", EcsSpec::Custom(GLOBAL_ECS)),
            group("parallel", &["slow", "fast"], &[], "parallel"),
        ]
        .concat(),
        "parallel",
    );
    let core = controlled_core(&config, false);
    let request = observed_request(request("race-ecs.example.", RecordType::A));
    resolve_and_commit(&core, &request).await;
    let before = facts(&request).await;
    exchange(
        &before.upstream,
        "fast",
        present(GLOBAL_ECS, 0),
        EcsPolicySource::Upstream,
        EcsValueSource::Custom,
    );
    drain(&core).await;
    assert_eq!(facts(&request).await, before);
}

#[tokio::test]
async fn group_all_failures_do_not_invent_a_selected_leaf() {
    let config = fixture(
        EcsSpec::Client,
        [
            doh("one", "fail.example.test", EcsSpec::Inherit),
            doh("two", "fail.example.test", EcsSpec::Disabled),
            group("group", &["one", "two"], &[], "parallel"),
        ]
        .concat(),
        "group",
    );
    let core = controlled_core(&config, true);
    let request = observed_request(request("failed-ecs.example.", RecordType::A));
    let completion = core.resolve_with_completion(&request).await;
    assert!(matches!(completion.result, Ok(CoreOutcome::Response(_))));
    assert_eq!(
        facts(&request).await.upstream,
        EcsExchange::Unrecorded {
            reason: EcsUnrecordedReason::NoSelectedResponse
        }
    );
}

#[tokio::test]
async fn fresh_cache_uses_probe_range_and_does_not_claim_an_exchange() {
    for (subnet, mode, kind, cidr) in [
        (
            Some("203.0.113.99/32"),
            EcsSpec::Client,
            EcsCacheMatchKind::Covering,
            Some("0.0.0.0/0"),
        ),
        (
            Some("0.0.0.0/0"),
            EcsSpec::Client,
            EcsCacheMatchKind::Exact,
            Some("0.0.0.0/0"),
        ),
        (None, EcsSpec::Disabled, EcsCacheMatchKind::NoEcs, None),
    ] {
        let config = fixture(
            mode,
            doh("remote", "ok.example.test", EcsSpec::Inherit),
            "remote",
        );
        let core = controlled_core(&config, false);
        let request = subnet.map_or_else(
            || request("cache-ecs.example.", RecordType::A),
            |subnet| request_with_ecs("cache-ecs.example.", subnet),
        );
        resolve_and_commit(&core, &request).await;
        let request = observed_request(request);
        assert_fresh(&resolve_and_commit(&core, &request).await);
        let ecs = facts(&request).await;
        assert_eq!(
            ecs.upstream,
            EcsExchange::NotSent {
                reason: EcsNotSentReason::CacheHit
            }
        );
        assert_eq!(
            ecs.cache_match,
            Some(EcsCacheMatch {
                kind,
                cidr: cidr.map(str::to_owned)
            })
        );
        assert!(ecs.refresh.is_none());
    }
}

#[tokio::test]
async fn single_flight_follower_has_no_independent_exchange_or_probe_match() {
    let config = fixture(
        EcsSpec::Client,
        doh("remote", "ok.example.test", EcsSpec::Inherit),
        "remote",
    );
    let core = controlled_core(&config, false);
    let leader = observed_request(request("shared-ecs.example.", RecordType::A));
    let mut completion = core.resolve_with_completion(&leader).await;
    let follower = observed_request(request("shared-ecs.example.", RecordType::A));
    let follow = core.resolve_with_completion(&follower);
    let commit = async {
        tokio::task::yield_now().await;
        completion
            .cache_commit
            .take()
            .unwrap()
            .commit(Duration::from_secs(1))
            .await;
    };
    let (result, _) = tokio::join!(follow, commit);
    assert_eq!(result.observation.unwrap().source, StatsSource::Cache);
    let ecs = facts(&follower).await;
    assert_eq!(
        ecs.upstream,
        EcsExchange::NotSent {
            reason: EcsNotSentReason::SingleFlight
        }
    );
    assert!(ecs.cache_match.is_none());
}

#[tokio::test]
async fn refresh_uses_new_decision_without_overwriting_front_or_cached_range() {
    let (old, latest, _, _, request, _, latest_key) =
        cross_runtime_pair("query-ecs-cross-stale").await;
    force_stale(&latest, &latest_key).await;
    let request = observed_request(request);
    let observation = resolve_and_commit(&old, &request).await;
    assert_eq!(observation.cache_status, CacheStatus::Stale);
    drain(&latest).await;
    let ecs = facts(&request).await;
    assert_eq!(
        ecs.upstream,
        EcsExchange::NotSent {
            reason: EcsNotSentReason::CacheHit
        }
    );
    assert_eq!(
        ecs.cache_match.as_ref().unwrap().kind,
        EcsCacheMatchKind::Covering
    );
    assert_eq!(
        ecs.cache_match.as_ref().unwrap().cidr.as_deref(),
        Some(MEMBER_ECS)
    );
    let EcsExchange::Observed { query, .. } = ecs.refresh.unwrap() else {
        panic!("missing refresh exchange")
    };
    assert_eq!(query, present("198.51.100.128/25", 0));
}

#[tokio::test]
async fn skipped_refresh_is_explicitly_not_started_and_details_off_stays_empty() {
    let (old, latest, _, _, request, _, _) = cross_runtime_pair("query-ecs-cross-fresh").await;
    assert!(!request.context.meta.completion.ecs_enabled());
    let request = observed_request(request);
    resolve_and_commit(&old, &request).await;
    drain(&latest).await;
    assert_eq!(
        facts(&request).await.refresh,
        Some(EcsExchange::NotSent {
            reason: EcsNotSentReason::NotStarted
        })
    );
    let plain = super::request("details-off.example.", RecordType::A);
    resolve_and_commit(&old, &plain).await;
    assert!(
        plain
            .context
            .meta
            .completion
            .settled_observation()
            .await
            .ecs
            .is_none()
    );
}

#[tokio::test]
async fn absent_response_and_missing_response_are_distinct_facts() {
    let config = fixture(
        EcsSpec::Client,
        doh("remote", "ok.example.test", EcsSpec::Inherit),
        "remote",
    );
    let no_ecs_core = core_with_transport(&config, Arc::new(RecordingDohTransport::new(45)));
    let request = observed_request(request("absent-response.example.", RecordType::A));
    resolve_and_commit(&no_ecs_core, &request).await;
    let EcsExchange::Observed {
        query, response, ..
    } = facts(&request).await.upstream
    else {
        panic!("expected observed exchange")
    };
    assert_eq!(query, present("192.0.2.0/24", 0));
    assert_eq!(response, EcsValue::Absent);
    let failed = controlled_core(&config, true);
    let failed_request = observed_request(super::request("no-response.example.", RecordType::A));
    failed.resolve_with_completion(&failed_request).await;
    let EcsExchange::Observed {
        query, response, ..
    } = facts(&failed_request).await.upstream
    else {
        panic!("expected observed query")
    };
    assert_eq!(query, present("192.0.2.0/24", 0));
    assert_eq!(response, EcsValue::Unrecorded);
}

#[tokio::test]
async fn heterogeneous_members_cache_match_never_fabricates_a_single_cidr() {
    let config = fixture(
        EcsSpec::Disabled,
        [
            doh("one", "ok.example.test", EcsSpec::Custom(GLOBAL_ECS)),
            doh("two", "ok.example.test", EcsSpec::Custom("2001:db8::/48")),
            group("group", &["one", "two"], &[], "failover"),
        ]
        .concat(),
        "group",
    );
    let core = controlled_core(&config, false);
    let request = super::request("members-ecs.example.", RecordType::A);
    resolve_and_commit(&core, &request).await;
    let request = observed_request(request);
    assert_fresh(&resolve_and_commit(&core, &request).await);
    assert_eq!(
        facts(&request).await.cache_match,
        Some(EcsCacheMatch {
            kind: EcsCacheMatchKind::Members,
            cidr: None
        })
    );
}

/// 断言真正发出的 DNS wire，而不是只检查预备 query 或成功应答。
#[tokio::test]
async fn client_or_custom_request_priority_and_fallback_match_wire_and_facts() {
    for (supplied, fallback, expected, source) in [
        (
            Some(("198.51.100.99", 32, 7)),
            "203.0.113.99/28",
            "198.51.100.0/24",
            EcsValueSource::ClientEcs,
        ),
        (
            Some(("2001:db8:1234:5678::1", 128, 64)),
            "203.0.113.99/28",
            "2001:db8:1234:5600::/56",
            EcsValueSource::ClientEcs,
        ),
        (
            Some(("198.51.100.99", 16, 0)),
            "203.0.113.99/28",
            "198.51.0.0/16",
            EcsValueSource::ClientEcs,
        ),
        (
            Some(("2001:db8:1234::1", 32, 0)),
            "203.0.113.99/28",
            "2001:db8::/32",
            EcsValueSource::ClientEcs,
        ),
        (
            Some(("198.51.100.99", 0, 0)),
            "203.0.113.99/28",
            "0.0.0.0/0",
            EcsValueSource::ClientEcs,
        ),
        (
            Some(("2001:db8::1", 0, 0)),
            "203.0.113.99/28",
            "::/0",
            EcsValueSource::ClientEcs,
        ),
        (
            None,
            "203.0.113.99/28",
            "203.0.113.96/28",
            EcsValueSource::Custom,
        ),
        (
            None,
            "2001:db8:1234:5678::99/64",
            "2001:db8:1234:5678::/64",
            EcsValueSource::Custom,
        ),
        (
            Some(("198.51.100.99", 33, 0)),
            "203.0.113.99/28",
            "203.0.113.96/28",
            EcsValueSource::Custom,
        ),
        (
            Some(("198.51.100.99", 24, 33)),
            "203.0.113.99/28",
            "203.0.113.96/28",
            EcsValueSource::Custom,
        ),
        (
            Some(("2001:db8::1", 129, 0)),
            "2001:db8::99/64",
            "2001:db8::/64",
            EcsValueSource::Custom,
        ),
        (
            Some(("2001:db8::1", 56, 129)),
            "2001:db8::99/64",
            "2001:db8::/64",
            EcsValueSource::Custom,
        ),
    ] {
        for client_addr in [Some("192.0.2.10".parse().unwrap()), None] {
            let config = fixture(
                EcsSpec::ClientOrCustom(fallback),
                doh("remote", "ok.example.test", EcsSpec::Inherit),
                "remote",
            );
            let transport = Arc::new(RecordingDohTransport::new(46));
            let core = core_with_transport(&config, Arc::clone(&transport));
            let mut input = request("client-or-custom.example.", RecordType::A);
            input.context.client.client_addr = client_addr;
            input.query =
                input
                    .query
                    .with_edns_client_subnet(supplied.map(|(addr, prefix, scope)| {
                        ClientSubnet::new(addr.parse().unwrap(), prefix, scope)
                    }));
            let input = observed_request(input);
            assert_upstream(&resolve_and_commit(&core, &input).await, CacheStatus::Miss);
            let ecs = facts(&input).await;
            exchange(
                &ecs.upstream,
                "remote",
                present(expected, 0),
                EcsPolicySource::Global,
                source,
            );
            let EcsExchange::Observed { policy, .. } = &ecs.upstream else {
                unreachable!()
            };
            assert_eq!(policy.mode, EcsPolicyMode::ClientOrCustom);
            assert_eq!(
                QueryEcs::decode_json(&ecs.encode_json().unwrap()).unwrap(),
                ecs
            );
            let wires = transport.wires();
            assert_eq!(wires.len(), 1);
            let net: ipnet::IpNet = expected.parse().unwrap();
            assert_eq!(
                wire_ecs(&wires[0].query),
                Some((net.network(), net.prefix_len()))
            );
            let EdnsOption::Subnet(sent) = wires[0]
                .query
                .edns
                .as_ref()
                .unwrap()
                .option(EdnsCode::Subnet)
                .unwrap()
            else {
                unreachable!()
            };
            assert_eq!(sent.scope_prefix(), 0);
        }
    }
}

#[tokio::test]
async fn client_or_custom_is_available_at_every_policy_source() {
    for expected in [
        EcsPolicySource::Global,
        EcsPolicySource::Upstream,
        EcsPolicySource::Client,
        EcsPolicySource::Strategy,
        EcsPolicySource::Rule,
    ] {
        let mode = EcsSpec::ClientOrCustom(MEMBER_ECS);
        let config = load_fixture(Fixture {
            name: "client-or-custom-sources",
            global_ecs: if expected == EcsPolicySource::Global {
                mode
            } else {
                EcsSpec::Custom(GLOBAL_ECS)
            },
            upstreams: doh(
                "remote",
                "ok.example.test",
                if expected == EcsPolicySource::Upstream {
                    mode
                } else {
                    EcsSpec::Inherit
                },
            ),
            default_upstream: "remote",
            strategy_ecs: if expected == EcsPolicySource::Strategy {
                mode
            } else {
                EcsSpec::Inherit
            },
            rules: if expected == EcsPolicySource::Rule {
                format!(
                    "      - rule_set: routed\n        upstream: remote\n{}",
                    mode.yaml("        ")
                )
            } else {
                String::new()
            },
            rule_sets: if expected == EcsPolicySource::Rule {
                "  - {type: const, name: routed, format: clash, rule: 'DOMAIN-SUFFIX,source.example'}\n".into()
            } else {
                String::new()
            },
            clients: if expected == EcsPolicySource::Client {
                format!(
                    "  - name: client\n    client_id: client-id\n    match: {{ips: [192.0.2.0/24]}}\n{}",
                    mode.yaml("    ")
                )
            } else {
                String::new()
            },
        });
        for supplied in [None, Some("192.0.2.99/32")] {
            let core = controlled_core(&config, false);
            let input = supplied.map_or_else(
                || request("source.example.", RecordType::A),
                |ecs| request_with_ecs("source.example.", ecs),
            );
            let input = observed_request(input);
            resolve_and_commit(&core, &input).await;
            let ecs = facts(&input).await;
            exchange(
                &ecs.upstream,
                "remote",
                present(
                    if supplied.is_some() {
                        "192.0.2.0/24"
                    } else {
                        MEMBER_ECS
                    },
                    0,
                ),
                expected,
                if supplied.is_some() {
                    EcsValueSource::ClientEcs
                } else {
                    EcsValueSource::Custom
                },
            );
            let EcsExchange::Observed { policy, .. } = ecs.upstream else {
                unreachable!()
            };
            assert_eq!(policy.mode, EcsPolicyMode::ClientOrCustom);
        }
    }
}

#[tokio::test]
async fn client_or_custom_cache_uses_final_ecs_for_both_branches() {
    let config = fixture(
        EcsSpec::ClientOrCustom("203.0.113.96/28"),
        doh("remote", "ok.example.test", EcsSpec::Inherit),
        "remote",
    );
    let transport = Arc::new(RecordingDohTransport::new(47));
    let core = core_with_transport(&config, Arc::clone(&transport));
    for supplied in [Some("198.51.100.99/32"), Some("192.0.2.99/32"), None] {
        let input = supplied.map_or_else(
            || request("cached-client-or-custom.example.", RecordType::A),
            |ecs| request_with_ecs("cached-client-or-custom.example.", ecs),
        );
        assert_upstream(&resolve_and_commit(&core, &input).await, CacheStatus::Miss);
        assert_fresh(&resolve_and_commit(&core, &input).await);
    }
    assert_eq!(transport.calls(), 3);
    let mut invalid = request("cached-client-or-custom.example.", RecordType::A);
    invalid.query = invalid
        .query
        .with_edns_client_subnet(Some(ClientSubnet::new(
            "198.51.100.99".parse().unwrap(),
            33,
            0,
        )));
    assert_fresh(&resolve_and_commit(&core, &invalid).await);
    assert_eq!(transport.calls(), 3);
    for (wire, expected) in
        transport
            .wires()
            .iter()
            .zip(["198.51.100.0/24", "192.0.2.0/24", "203.0.113.96/28"])
    {
        let net: ipnet::IpNet = expected.parse().unwrap();
        assert_eq!(
            wire_ecs(&wire.query),
            Some((net.network(), net.prefix_len()))
        );
    }
}

/// 相同请求 ECS 令成员变成 uniform；缺失时不同成员自定义值仍采用 members key。
#[tokio::test]
async fn client_or_custom_group_members_keep_uniform_and_heterogeneous_keys() {
    let config = fixture(
        EcsSpec::Disabled,
        [
            doh(
                "one",
                "ok.example.test",
                EcsSpec::ClientOrCustom(GLOBAL_ECS),
            ),
            doh(
                "two",
                "ok.example.test",
                EcsSpec::ClientOrCustom(MEMBER_ECS),
            ),
            group("group", &["one", "two"], &[], "failover"),
        ]
        .concat(),
        "group",
    );
    let transport = Arc::new(RecordingDohTransport::new(48));
    let core = core_with_transport(&config, Arc::clone(&transport));
    for supplied in [None, Some("192.0.2.99/32")] {
        let input = supplied.map_or_else(
            || request("group-client-or-custom.example.", RecordType::A),
            |ecs| request_with_ecs("group-client-or-custom.example.", ecs),
        );
        let first = observed_request(input.clone());
        assert_upstream(&resolve_and_commit(&core, &first).await, CacheStatus::Miss);
        exchange(
            &facts(&first).await.upstream,
            "one",
            present(
                if supplied.is_some() {
                    "192.0.2.0/24"
                } else {
                    GLOBAL_ECS
                },
                0,
            ),
            EcsPolicySource::Upstream,
            if supplied.is_some() {
                EcsValueSource::ClientEcs
            } else {
                EcsValueSource::Custom
            },
        );
        let second = observed_request(input);
        assert_fresh(&resolve_and_commit(&core, &second).await);
        assert_eq!(
            facts(&second).await.cache_match.unwrap().kind,
            if supplied.is_some() {
                EcsCacheMatchKind::Covering
            } else {
                EcsCacheMatchKind::Members
            }
        );
    }
    assert_eq!(transport.calls(), 2);
}

#[tokio::test]
async fn client_or_custom_refresh_reuses_the_selected_branch() {
    for supplied in [None, Some("198.51.100.99/32")] {
        let config = fixture(
            EcsSpec::ClientOrCustom(GLOBAL_ECS),
            doh("remote", "ok.example.test", EcsSpec::Inherit),
            "remote",
        );
        let transport = Arc::new(RecordingDohTransport::new(49));
        let core = core_with_transport(&config, Arc::clone(&transport));
        let input = supplied.map_or_else(
            || request("refresh-client-or-custom.example.", RecordType::A),
            |ecs| request_with_ecs("refresh-client-or-custom.example.", ecs),
        );
        resolve_and_commit(&core, &input).await;
        let key = cache_key(&core, &plan_for(&core, &input), &input).unwrap();
        force_stale(&core, &key).await;
        let _cell = publish_current(&core, 1);
        let input = observed_request(input);
        assert_eq!(
            resolve_and_commit(&core, &input).await.cache_status,
            CacheStatus::Stale
        );
        drain(&core).await;
        let ecs = facts(&input).await;
        assert_eq!(
            ecs.upstream,
            EcsExchange::NotSent {
                reason: EcsNotSentReason::CacheHit
            }
        );
        exchange(
            ecs.refresh.as_ref().unwrap(),
            "remote",
            present(
                if supplied.is_some() {
                    "198.51.100.0/24"
                } else {
                    GLOBAL_ECS
                },
                0,
            ),
            EcsPolicySource::Global,
            if supplied.is_some() {
                EcsValueSource::ClientEcs
            } else {
                EcsValueSource::Custom
            },
        );
        let EcsExchange::Observed { policy, .. } = ecs.refresh.unwrap() else {
            unreachable!()
        };
        assert_eq!(policy.mode, EcsPolicyMode::ClientOrCustom);
        assert_eq!(transport.calls(), 2);
    }
}
