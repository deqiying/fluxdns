use super::*;

const FIXTURE: &str = include_str!("../../../tests/fixtures/config-v2.yaml");

fn fixture() -> ConfigV2 {
    ConfigV2::parse(FIXTURE.as_bytes()).unwrap()
}

#[test]
fn strict_v2_fixture_and_defaults() {
    let config = fixture();
    assert!(config.dns.cache.is_none());
    assert!(config.dns.resolve_log.is_none());
    let cache = GlobalCacheV2::default();
    assert!(!cache.enabled);
    assert!(!cache.persistence.enabled);
    assert_eq!(
        cache.persistence.snapshot_interval,
        Duration::from_secs(300)
    );
    assert_eq!(cache.memory.max_size_bytes, 64 * 1024 * 1024);
    assert_eq!(config.statistics.retention.days, 7);
    assert_eq!(config.statistics.retention.grace_days, 3);
    assert_eq!(config.statistics.retention.reference_size_bytes, 1 << 30);
    assert_eq!(
        config.clients[0].r#match.ips[0].to_string(),
        "192.0.2.10/32"
    );
}

#[test]
fn old_fields_unknown_fields_and_null_are_rejected() {
    for (from, to) in [
        ("version: 2", "version: 1"),
        (
            "dns: {}",
            "dns: {resolve_log: {enable: true, max_records: 100}}",
        ),
        ("dns: {}", "dns: {cache: null}"),
        ("dns: {}", "dns: {resolve_log: null}"),
        ("dns: {}", "dns: {ttl_override: null}"),
        ("dns: {}", "dns: {edns_client_subnet: null}"),
        ("    type: udp", "    type: udp\n    enable: true"),
        ("    type: udp", "    type: udp\n    routes: []"),
        (
            "    client_id: Desktop-01",
            "    client_id: Desktop-01\n    extra: true",
        ),
        ("      ips:", "      ids: [old-id]\n      ips:"),
        ("  records_path: ./data/queries", "  records_path: null"),
    ] {
        assert!(
            ConfigV2::parse(FIXTURE.replace(from, to).as_bytes()).is_err(),
            "{to}"
        );
    }
    assert!(yaml_serde::from_str::<SnapshotV2>("max_size_bytes: 1024").is_err());
    assert!(yaml_serde::from_str::<ResolveLogV2>("{}").is_err());
    assert!(ConfigV2::parse(&vec![b' '; MAX_CONFIG_BYTES + 1]).is_err());
}

#[test]
fn namespaces_identity_case_and_cidr_rules() {
    let mut config = fixture();
    // listener 和 upstream 同名合法；客户端 name 与 client_id 各自唯一。
    config.validate().unwrap();
    config.clients.push(config.clients[0].clone());
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("clients[1].name"));
    assert!(error.contains("clients[1].client_id"));
    config.clients[1].name = "mobile".into();
    config.clients[1].client_id = "desktop-01".into();
    config.clients[1].r#match.ips = vec![parse_client_ip("192.0.2.0/24").unwrap()];
    config.validate().unwrap();
    config.clients[1].r#match.ips = vec![parse_client_ip("::ffff:192.0.2.10/128").unwrap()];
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate normalized CIDR")
    );
    for value in [
        "",
        "bad id",
        "a/b",
        "a%2Fb",
        "a?b",
        "中文",
        &"a".repeat(129),
    ] {
        assert!(!valid_client_id(value));
    }
    assert!(valid_client_id("Abc-._~9"));
    assert_eq!(
        parse_client_ip("192.0.2.19/24").unwrap().to_string(),
        "192.0.2.0/24"
    );
    assert!(parse_client_ip("::ffff:192.0.2.10/80").is_err());
    assert!(parse_client_ip("192.0.2.1/99").is_err());
}

#[test]
fn shared_reference_graph_and_variant_validation_still_apply() {
    let mut config = fixture();
    config.strategy[0].default_upstream = "missing".into();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("default_upstream")
    );
    config.strategy[0].default_upstream = "local".into();
    config.upstreams = yaml_serde::from_str(
        r#"
- {name: local, type: group, upstreams: [{name: other}], upstream_mode: parallel, timeout: 1s}
- {name: other, type: group, upstreams: [{name: local}], upstream_mode: parallel, timeout: 1s}
"#,
    )
    .unwrap();
    assert!(config.validate().unwrap_err().to_string().contains("cycle"));
}

#[test]
fn retention_and_snapshot_bounds_are_checked() {
    let mut config = fixture();
    for (days, grace, bytes) in [
        (0, 3, 1),
        (3650, 1, 1),
        (7, u32::MAX, 1),
        (7, 3, 0),
        (7, 3, MAX_SIZE_BYTES + 1),
    ] {
        config.statistics.retention = RetentionV2 {
            days,
            grace_days: grace,
            reference_size_bytes: bytes,
        };
        assert!(config.validate().is_err());
    }
    config.statistics.retention = RetentionV2 {
        days: 3650,
        grace_days: 0,
        reference_size_bytes: MAX_SIZE_BYTES,
    };
    config.validate().unwrap();
    config.dns.cache = Some(GlobalCacheV2::default());
    for seconds in [0, 86401] {
        config
            .dns
            .cache
            .as_mut()
            .unwrap()
            .persistence
            .snapshot_interval = Duration::from_secs(seconds);
        assert!(config.validate().is_err());
    }
    assert!(
        yaml_serde::from_str::<SnapshotV2>("snapshot_interval: 184467440737095516160s").is_err()
    );
    assert!(yaml_serde::from_str::<RetentionV2>("days: -1").is_err());
}

#[test]
fn two_level_paths_and_lexical_collisions() {
    let root = PathBuf::from(super::super::test_support::absolute_path("v2-contract"));
    let source = root.join("source/config.yaml");
    let mut config = fixture();
    config.work.path = "../work".into();
    let paths = config.resolve_paths(&source).unwrap();
    assert_eq!(paths.work, root.join("work"));
    assert_eq!(paths.records, root.join("work/data/queries"));
    for path in [
        "./data/queries/other.db",
        "./data/../data/statistics.sqlite3",
        "./data/statistics.sqlite3/cache.db",
        "./data",
        "./config.yaml",
        "./config.yaml/cache.db",
        "./logs/fluxdns.log",
    ] {
        let mut cache = GlobalCacheV2::default();
        cache.persistence.path = path.into();
        config.dns.cache = Some(cache);
        assert!(config.resolve_paths(&source).is_err(), "{path}");
    }
    config.dns.cache = None;
    for path in [
        "./data/statistics.sqlite3/records",
        "./logs/fluxdns.log/records",
        "./config.yaml/records",
    ] {
        config.database.records_path = path.into();
        assert!(config.resolve_paths(&source).is_err(), "{path}");
    }
    assert!(
        config
            .resolve_paths(Path::new("relative/config.yaml"))
            .is_err()
    );
}

#[test]
fn inheritance_and_redaction_are_not_replaced_by_effective_values() {
    let config = fixture();
    assert!(config.clients[0].cache.is_none());
    let disabled = FIXTURE.replace(
        "    client_id: Desktop-01",
        "    client_id: Desktop-01\n    cache: {enabled: false}",
    );
    let config = ConfigV2::parse(disabled.as_bytes()).unwrap();
    assert_eq!(
        config.clients[0].cache.as_ref().unwrap().enabled,
        Some(false)
    );
    assert!(!format!("{config:?}").contains("Desktop-01"));
}
