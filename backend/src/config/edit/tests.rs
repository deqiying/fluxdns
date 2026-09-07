use super::*;

const FIXTURE: &str = include_str!("../../../tests/fixtures/config-v2.yaml");

fn build(source: &str, changes: &[ConfigChange]) -> Result<SourceCandidate, EditError> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p1-edit/source.yaml");
    build_candidate(source, &path, changes)
}

fn changes(json: &str) -> Vec<ConfigChange> {
    serde_json::from_str(json).unwrap()
}

#[test]
fn rename_updates_only_typed_references_and_preserves_unrelated_source() {
    let source = FIXTURE.replace(
        "    default_upstream: local",
        "    default_upstream: local # keep reference comment",
    );
    let candidate = build(&source, &changes(r#"[
      {"module":"upstreams","change":{"action":"update","original_name":"local","value":{"name":"new","type":"hosts","format":"hosts","hosts":"127.0.0.1 localhost"}}}
    ]"#)).unwrap();
    assert!(candidate.renamed);
    assert_eq!(candidate.config.strategy[0].default_upstream, "new");
    assert_eq!(candidate.config.hosts[0].name(), "local");
    assert_eq!(candidate.config.listener[0].name(), "local");
    assert!(candidate.source.contains("# keep reference comment"));
    assert!(candidate.source.contains("hosts: \"127.0.0.1 localhost\""));
    assert!(candidate.source.contains("path: ./data/statistics.sqlite3"));
    assert!(
        candidate
            .source
            .contains("ips: [192.0.2.10, \"2001:db8::/64\"]")
    );
}

#[test]
fn client_rename_preserves_id_implicit_defaults_and_comments() {
    let candidate = build(FIXTURE, &changes(r#"[
      {"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"new","match":{"ips":["192.0.2.10/32","2001:db8::/64"]}}}}
    ]"#)).unwrap();
    assert_eq!(candidate.config.clients[0].client_id, "Desktop-01");
    assert!(candidate.config.clients[0].cache.is_none());
    assert!(
        candidate
            .source
            .contains("ips: [192.0.2.10, \"2001:db8::/64\"]")
    );
    assert!(!candidate.source.contains("cache:"));
}

#[test]
fn validates_complete_candidate_not_just_shape() {
    for json in [
        r#"[{"module":"listener","change":{"action":"update","original_name":"local","value":{"name":"local","type":"udp","addresses":["127.0.0.1"],"port":15353,"strategy":"missing"}}}]"#,
        r#"[{"module":"upstreams","change":{"action":"create","value":{"name":"local","type":"hosts","format":"hosts","hosts":"127.0.0.2 local"}}}]"#,
        r#"[{"module":"statistics","change":{"retention":{"days":0,"grace_days":3,"reference_size_bytes":1}}}]"#,
        r#"[{"module":"clients","change":{"action":"create","value":{"name":"other","client_id":"Desktop-01"}}}]"#,
        r#"[{"module":"clients","change":{"action":"create","value":{"name":"other","client_id":"Other","match":{"ips":["::ffff:192.0.2.10"]}}}}]"#,
    ] {
        assert!(
            matches!(
                build(FIXTURE, &changes(json)),
                Err(EditError::Validation(_))
            ),
            "{json}"
        );
    }
}

#[test]
fn rejects_unknown_old_keys_duplicate_targets_cycles_and_path_conflicts() {
    let one = changes(
        r#"[{"module":"upstreams","change":{"action":"update","original_name":"absent","value":{"name":"new","type":"hosts","format":"hosts","hosts":"127.0.0.1 localhost"}}}]"#,
    );
    assert!(matches!(build(FIXTURE, &one), Err(EditError::NotFound)));
    let one = ConfigChange::Logs(ConfigV2::parse(FIXTURE.as_bytes()).unwrap().logs);
    assert!(matches!(
        build(FIXTURE, &[one.clone(), one]),
        Err(EditError::DuplicateTarget)
    ));
    let cycle = changes(
        r#"[{"module":"upstreams","change":{"action":"update","original_name":"local","value":{"name":"local","type":"group","upstreams":[{"name":"local"}],"upstream_mode":"parallel","timeout":"1s"}}}]"#,
    );
    assert!(matches!(
        build(FIXTURE, &cycle),
        Err(EditError::Validation(_))
    ));
    let collision = changes(
        r#"[{"module":"logs","change":{"enable":true,"level":"info","path":"./data/statistics.sqlite3"}}]"#,
    );
    assert!(matches!(
        build(FIXTURE, &collision),
        Err(EditError::Validation(_))
    ));
}

#[test]
fn combined_creation_resolves_forward_references_and_variant_fields_are_removed() {
    let candidate = build(FIXTURE, &changes(r#"[
      {"module":"strategy","change":{"action":"update","original_name":"default","value":{"name":"default","rules":[{"hosts":"local"}],"default_upstream":"group"}}},
      {"module":"upstreams","change":{"action":"create","value":{"name":"group","type":"group","upstreams":[{"name":"local"}],"upstream_mode":"parallel","timeout":"1s"}}},
      {"module":"upstreams","change":{"action":"update","original_name":"local","value":{"name":"local","type":"doh","address":"https://dns.example/dns-query","connect_ip":"192.0.2.1"}}}
    ]"#)).unwrap();
    assert_eq!(candidate.config.strategy[0].default_upstream, "group");
    assert!(matches!(
        candidate.config.upstreams[0],
        UpstreamDto::Doh { .. }
    ));
    let tree: Value = yaml_serde::from_str(&candidate.source).unwrap();
    assert!(tree["upstreams"][0].get("hosts").is_none());
    assert!(tree["upstreams"][0].get("format").is_none());
}

#[test]
fn missing_and_explicit_disabled_are_distinct() {
    let candidate = build(FIXTURE, &changes(r#"[{"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"desktop","match":{"ips":["192.0.2.10","2001:db8::/64"]},"cache":{"enabled":false}}}}]"#)).unwrap();
    assert_eq!(
        candidate.config.clients[0].cache.as_ref().unwrap().enabled,
        Some(false)
    );
    let next = build(&candidate.source, &changes(r#"[{"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"desktop","match":{"ips":["192.0.2.10","2001:db8::/64"]}}}}]"#)).unwrap();
    assert!(next.config.clients[0].cache.is_none());
    assert!(!next.source.contains("null"));
}

#[test]
fn flow_documents_are_supported_and_aliases_rejected_without_fallback() {
    let tree: Value = yaml_serde::from_str(FIXTURE).unwrap();
    let flow = serde_json::to_string(&tree).unwrap();
    let change = changes(
        r#"[{"module":"logs","change":{"enable":true,"level":"warn","path":"./logs/other.log"}}]"#,
    );
    assert!(build(&flow, &change).unwrap().config.logs.enable);
    let anchored = FIXTURE.replace("strategy: default", "strategy: &strategy default");
    assert!(matches!(
        build(&anchored, &change),
        Err(EditError::UnsupportedSource)
    ));
}

#[test]
fn budget_and_readonly_payloads_are_rejected() {
    assert!(matches!(build(FIXTURE, &[]), Err(EditError::Budget)));
    let change = changes(
        r#"[{"module":"logs","change":{"enable":true,"level":"info","path":"./logs/other.log"}}]"#,
    );
    assert!(matches!(
        build(FIXTURE, &vec![change[0].clone(); 129]),
        Err(EditError::Budget)
    ));
    for json in [
        r#"[{"module":"database","change":{"path":"elsewhere"}}]"#,
        r#"[{"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"new","client_id":"New"}}}]"#,
        r#"[{"module":"logs","change":{"enable":true,"level":"info","path":"./logs/other.log","users":[]}}]"#,
    ] {
        assert!(serde_json::from_str::<Vec<ConfigChange>>(json).is_err());
    }
}

#[test]
fn simultaneous_renames_are_not_chained_and_keep_comments_on_other_entries() {
    let source = FIXTURE.replace(
        "strategy:\n  - name: default",
        "  # untouched upstream comment\n  - {name: other, type: hosts, format: hosts, hosts: '127.0.0.2 other'}\nstrategy:\n  - name: default",
    );
    let candidate = build(&source, &changes(r#"[
      {"module":"upstreams","change":{"action":"update","original_name":"local","value":{"name":"other","type":"hosts","format":"hosts","hosts":"127.0.0.1 localhost"}}},
      {"module":"upstreams","change":{"action":"update","original_name":"other","value":{"name":"local","type":"hosts","format":"hosts","hosts":"127.0.0.2 other"}}}
    ]"#)).unwrap();
    assert_eq!(candidate.config.upstreams[0].name(), "other");
    assert_eq!(candidate.config.upstreams[1].name(), "local");
    assert_eq!(candidate.config.strategy[0].default_upstream, "other");
    assert!(candidate.source.contains("# untouched upstream comment"));
    assert!(candidate.source.starts_with("# v2"));
}

#[test]
fn all_reference_kinds_follow_namespace_and_preserve_selector_and_secret_expression() {
    let source = format!(
        "{}\n{}",
        FIXTURE
            .replace(
                "      - hosts: local",
                "      - rule_set: local:!CN\n        upstream: group",
            )
            .replace(
                "    client_id: Desktop-01",
                "    client_id: Desktop-01\n    strategy: default"
            )
            .replace(
                "strategy:\n  - name: default",
                r#"  - name: doh
    type: doh
    address: https://dns.example/dns-query
    bootstrap: local
    proxy: local
  - name: group
    type: group
    upstreams: [{name: local}, {name: doh}]
    upstream_mode: parallel
    timeout: 1s
    fallbacks: [{name: local}]
    fallback_upstream_mode: parallel
    fallback_timeout: 2s
strategy:
  - name: default"#
            ),
        r#"outbound:
  - {name: local, type: socks5, proxy_url: {env: LOCAL_PROXY}}
rule_set:
  - {name: local, type: remote, format: dat, url: 'https://rules.example/local.dat', proxy: local}
"#
    );
    let config = ConfigV2::parse(source.as_bytes()).unwrap();
    let mut upstream = config.upstreams[0].clone();
    if let UpstreamDto::Hosts { name, .. } = &mut upstream {
        *name = "renamed-upstream".into();
    }
    let mut strategy = config.strategy[0].clone();
    strategy.name = "renamed-strategy".into();
    let mut outbound = config.outbound[0].clone();
    outbound.name = "renamed-proxy".into();
    let mut rule_set = config.rule_set[0].clone();
    if let RuleSetDto::Remote { name, .. } = &mut rule_set {
        *name = "renamed-rules".into();
    }
    let candidate = build(
        &source,
        &[
            ConfigChange::Upstreams(ResourceMutation::Update {
                original_name: "local".into(),
                value: upstream,
            }),
            ConfigChange::Strategy(ResourceMutation::Update {
                original_name: "default".into(),
                value: strategy,
            }),
            ConfigChange::Outbound(ResourceMutation::Update {
                original_name: "local".into(),
                value: outbound,
            }),
            ConfigChange::RuleSet(ResourceMutation::Update {
                original_name: "local".into(),
                value: rule_set,
            }),
        ],
    )
    .unwrap();
    let tree: Value = yaml_serde::from_str(&candidate.source).unwrap();
    assert_eq!(tree["listener"][0]["strategy"], "renamed-strategy");
    assert_eq!(tree["upstreams"][1]["bootstrap"], "renamed-upstream");
    assert_eq!(tree["upstreams"][1]["proxy"], "renamed-proxy");
    assert_eq!(
        tree["upstreams"][2]["upstreams"][0]["name"],
        "renamed-upstream"
    );
    assert_eq!(
        tree["upstreams"][2]["fallbacks"][0]["name"],
        "renamed-upstream"
    );
    assert_eq!(
        tree["strategy"][0]["rules"][0]["rule_set"],
        "renamed-rules:!CN"
    );
    assert_eq!(tree["rule_set"][0]["proxy"], "renamed-proxy");
    assert_eq!(tree["clients"][0]["strategy"], "renamed-strategy");
    assert!(candidate.source.contains("timeout: 1s"));
    assert!(candidate.source.contains("fallback_timeout: 2s"));
    assert!(candidate.source.contains("proxy_url: {env: LOCAL_PROXY}"));
    assert!(candidate.source.contains("https://rules.example/local.dat"));
}

#[test]
fn editing_is_repeatable_for_crlf_block_text_and_empty_collections() {
    let source = FIXTURE
        .replace("dns: {}", "dns: {}\noutbound: []")
        .replace(
            "hosts: \"127.0.0.1 localhost\"",
            "hosts: |\n      127.0.0.1 localhost",
        )
        .replace('\n', "\r\n");
    let config = ConfigV2::parse(source.as_bytes()).unwrap();
    let mut logs = config.logs;
    logs.enable = true;
    let candidate = build(&source, &[ConfigChange::Logs(logs.clone())]).unwrap();
    assert!(
        candidate
            .source
            .contains("hosts: |\r\n      127.0.0.1 localhost")
    );
    let other = changes(
        r#"[{"module":"outbound","change":{"action":"create","value":{"name":"proxy","type":"socks5","proxy_url":{"env":"PROXY_URL"}}}}]"#,
    );
    let next = build(&candidate.source, &other).unwrap();
    logs.level = crate::config::model::LogLevelDto::Debug;
    assert!(build(&next.source, &[ConfigChange::Logs(logs)]).is_ok());
}
