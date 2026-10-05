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
fn client_update_replaces_id_only_when_explicit() {
    let candidate = build(FIXTURE, &changes(r#"[
      {"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"desktop","client_id":"desktop-home","match":{"ips":["192.0.2.10","2001:db8::/64"]}}}}
    ]"#)).unwrap();
    assert_eq!(candidate.config.clients[0].client_id, "desktop-home");
    assert!(candidate.source.contains("desktop-home"));
    assert!(!candidate.source.contains("Desktop-01"));
}

#[test]
fn client_id_update_rejects_invalid_format_and_duplicates() {
    let invalid = build(
        FIXTURE,
        &changes(
            r#"[
      {"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"desktop","client_id":"has space/1"}}}
    ]"#,
        ),
    );
    assert!(matches!(invalid, Err(EditError::Validation(_))));

    let source = format!("{FIXTURE}\n  - name: laptop\n    client_id: Laptop-01\n");
    let duplicate = build(
        &source,
        &changes(
            r#"[
      {"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"desktop","client_id":"Laptop-01"}}}
    ]"#,
        ),
    );
    assert!(matches!(duplicate, Err(EditError::Validation(_))));

    // 两个客户端在同一候选里互换 ID 时，按整份候选校验，不因中间状态冲突而拒绝。
    let swapped = build(&source, &changes(r#"[
      {"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"desktop","client_id":"Laptop-01"}}},
      {"module":"clients","change":{"action":"update","original_name":"laptop","value":{"name":"laptop","client_id":"Desktop-01"}}}
    ]"#)).unwrap();
    assert_eq!(swapped.config.clients[0].client_id, "Laptop-01");
    assert_eq!(swapped.config.clients[1].client_id, "Desktop-01");
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
        r#"[{"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"new","client_ids":["New"]}}}]"#,
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

#[test]
fn redacted_inline_proxy_url_is_restored_when_the_edit_does_not_change_it() {
    const SOURCE: &str = r#"version: 2
work:
  path: .
  rules_path: ./rules
database:
  type: sqlite
  path: ./data/statistics.sqlite3
  records_path: ./data/queries
logs:
  enable: false
  level: info
  path: ./logs/fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 18080
dns: {}
listener:
  - name: dns
    type: udp
    addresses: [127.0.0.1]
    port: 15353
    strategy: default
outbound:
  - name: sg
    type: socks5
    proxy_url: socks5://user:secret@proxy.example:1080
strategy:
  - name: default
    rules:
      - hosts: local
    default_upstream: local
upstreams:
  - name: local
    type: hosts
    format: hosts
    hosts: "127.0.0.1 localhost"
hosts:
  - name: local
    type: const
    format: hosts
    hosts: "127.0.0.1 localhost"
"#;

    // 模拟 Management 读取：typed 值经序列化再反序列化后只携带脱敏密码。
    let config = ConfigV2::parse(SOURCE.as_bytes()).unwrap();
    let redacted = serde_json::to_value(&config.outbound[0]).unwrap()["proxy_url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        redacted,
        "socks5://user:FLUXDNS_REDACTED_SECRET@proxy.example:1080"
    );

    // 用户只改名，未重新输入 URL；展开步骤必须按原 name 找回原密码，而不是写回脱敏值。
    let mut renamed: OutboundDto =
        serde_json::from_value(serde_json::to_value(&config.outbound[0]).unwrap()).unwrap();
    assert_eq!(
        renamed.proxy_url,
        crate::config::model::SecretRefDto::Inline { url: redacted }
    );
    renamed.name = "sg-renamed".into();
    let expanded = expand_redacted_changes(
        SOURCE,
        &[ConfigChange::Outbound(ResourceMutation::Update {
            original_name: "sg".into(),
            value: renamed,
        })],
    )
    .unwrap();
    let candidate = build(SOURCE, &expanded).unwrap();
    assert!(
        candidate
            .source
            .contains("socks5://user:secret@proxy.example:1080")
    );
    assert!(!candidate.source.contains("FLUXDNS_REDACTED_SECRET"));
    assert_eq!(
        yaml_serde::from_str::<Value>(&candidate.source).unwrap()["outbound"][0]["name"],
        "sg-renamed"
    );

    // 主机改变后占位符无法按活动源恢复：必须报错让用户重填，不能把占位符写进配置文件。
    let mut moved: OutboundDto =
        serde_json::from_value(serde_json::to_value(&config.outbound[0]).unwrap()).unwrap();
    moved.proxy_url = crate::config::model::SecretRefDto::Inline {
        url: "socks5://user:FLUXDNS_REDACTED_SECRET@other.example:1080".into(),
    };
    let error = expand_redacted_changes(
        SOURCE,
        &[ConfigChange::Outbound(ResourceMutation::Update {
            original_name: "sg".into(),
            value: moved,
        })],
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("outbound[0].proxy_url"),
        "{error}"
    );

    // 活动源是同址内联来源时，未改动的密码仍可恢复。
    let unchanged: OutboundDto =
        serde_json::from_value(serde_json::to_value(&config.outbound[0]).unwrap()).unwrap();
    let expanded = expand_redacted_changes(
        SOURCE,
        &[ConfigChange::Outbound(ResourceMutation::Update {
            original_name: "sg".into(),
            value: unchanged,
        })],
    )
    .unwrap();
    let candidate = build(SOURCE, &expanded).unwrap();
    assert!(
        candidate
            .source
            .contains("socks5://user:secret@proxy.example:1080")
    );
}
