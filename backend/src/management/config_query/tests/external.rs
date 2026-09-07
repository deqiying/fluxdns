use super::*;
use crate::config::contract::{ConfigV2, MAX_CONFIG_BYTES};
use crate::management::config_query::external::external_diff;

fn source_tree() -> yaml_serde::Value {
    let mut tree: yaml_serde::Value = yaml_serde::from_str(SOURCE).unwrap();
    tree["outbound"] = yaml_serde::to_value(json!([{
        "name": "proxy", "type": "socks5", "proxy_url": {"env": "FLUXDNS_TEST_UNRESOLVED_PROXY"},
    }]))
    .unwrap();
    tree["rule_set"] = yaml_serde::to_value(json!([{
        "name": "remote-rules", "type": "file", "format": "json", "path": "./rules/before.json",
    }]))
    .unwrap();
    tree
}

fn encoded(tree: &yaml_serde::Value) -> String {
    yaml_serde::to_string(tree).unwrap()
}

fn project(fixture: &Fixture, tree: &yaml_serde::Value) -> Value {
    fs::write(&fixture.source, encoded(tree)).unwrap();
    serde_json::to_value(external_diff(fixture.store()).unwrap()).unwrap()
}

fn report(name: &str, samples: &[Value]) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("_fluxdns/p1-config-diff-projections");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(format!("{name}.json")),
        serde_json::to_vec(samples).unwrap(),
    )
    .unwrap();
}

#[test]
fn typed_preview_covers_each_editable_module_without_resolving_paths_or_secrets() {
    let mut tree = source_tree();
    let fixture = Fixture::with_source(1, &encoded(&tree));
    let initial = fixture.state();
    tree["listener"][0]["port"] = 15354.into();
    tree["upstreams"][0]["hosts"] = "127.0.0.2 localhost".into();
    tree["strategy"][0]["cache"] = yaml_serde::to_value(json!({"enabled": false})).unwrap();
    tree["hosts"][0]["hosts"] = "127.0.0.2 localhost".into();
    tree["outbound"][0]["proxy_url"] =
        yaml_serde::to_value(json!({"file":"./secrets/missing-proxy.txt"})).unwrap();
    tree["rule_set"][0]["path"] = "./rules/after.json".into();
    tree["clients"][0]["match"]["ips"] = yaml_serde::to_value(json!(["192.0.2.11"])).unwrap();
    tree["dns"]["resolve_log"] = yaml_serde::to_value(json!({"enable": true})).unwrap();
    tree["statistics"]["retention"]["days"] = 8.into();
    tree["logs"]["level"] = "warn".into();
    let result = project(&fixture, &tree);
    assert!(result["parse_error"].is_null());
    assert_eq!(result["protected_changes"], json!([]));
    assert_eq!(result["editable"].as_array().unwrap().len(), 10);
    assert_eq!(
        result["editable"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| pair["external"]["module"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "listener",
            "upstreams",
            "strategy",
            "hosts",
            "outbound",
            "rule_set",
            "clients",
            "dns",
            "statistics",
            "logs"
        ]
    );
    assert_eq!(result["editable"][2]["active"]["value"].get("cache"), None);
    assert_eq!(
        result["editable"][2]["external"]["value"]["cache"],
        json!({"enabled":false})
    );
    assert_eq!(
        result["editable"][4]["external"]["value"]["proxy_url"],
        json!({"file":"./secrets/missing-proxy.txt"})
    );
    assert!(!fixture.root.join("secrets").exists());
    assert!(!fixture.root.join("rules").exists());
    assert_eq!(
        fixture.state()["active_revision"],
        initial["active_revision"]
    );
    assert_eq!(
        fixture.state()["runtime_revision"],
        initial["runtime_revision"]
    );
    assert_eq!(
        result["expected"]["active_revision"],
        initial["active_revision"]
    );
    assert_eq!(
        result["expected"]["observed_file_revision"],
        fixture.state()["observed_file_revision"]
    );
    assert_eq!(
        fs::read_to_string(&fixture.derived).unwrap(),
        format!(
            "{}\n# private-configuration-sentinel\n",
            encoded(&source_tree())
        )
    );
    report("modules", &[result]);
}

#[test]
fn nested_listener_group_and_inline_variants_keep_typed_source_values() {
    let mut tree = source_tree();
    let fixture = Fixture::with_source(1, &encoded(&tree));
    tree["listener"].as_sequence_mut().unwrap().push(yaml_serde::to_value(json!({
        "name":"https","type":"doh","routes":[{"path":"/dns-query","strategy":"default"}],
        "endpoints":[{
            "name":"tls","addresses":["127.0.0.1"],"port":18443,
            "tls":{"mode":"terminate","certificate_file":"./certificates/missing.crt","private_key_file":"./certificates/missing.key"},
            "client_ip":{"source":"peer"},
        }],
    })).unwrap());
    tree["upstreams"].as_sequence_mut().unwrap().extend([
        yaml_serde::to_value(json!({
            "name":"doh","type":"doh","address":"https://dns.example.test/dns-query","connect_ip":"192.0.2.8",
        })).unwrap(),
        yaml_serde::to_value(json!({
            "name":"group","type":"group","upstreams":[{"name":"local","weight":2},{"name":"doh"}],
            "upstream_mode":"load-balance","timeout":"1s",
        })).unwrap(),
    ]);
    tree["rule_set"][0] = yaml_serde::to_value(json!({
        "name":"remote-rules","type":"const","format":"json","rule":"[\"example.test\"]",
    }))
    .unwrap();
    ConfigV2::parse(encoded(&tree).as_bytes()).unwrap();
    let result = project(&fixture, &tree);
    assert!(result["parse_error"].is_null());
    assert_eq!(result["editable"].as_array().unwrap().len(), 4);
    let listener = &result["editable"][0]["external"]["value"];
    assert_eq!(
        listener["endpoints"][0]["tls"]["private_key_file"],
        "./certificates/missing.key"
    );
    assert!(!fixture.root.join("certificates").exists());
    let group = &result["editable"][2]["external"]["value"];
    assert_eq!(group["upstreams"][0]["name"], "local");
    assert_eq!(group["upstreams"][0]["weight"], 2);
    assert_eq!(group["upstreams"][1]["name"], "doh");
    assert_eq!(group["timeout"], "1000000000ns");
    assert_eq!(result["editable"][3]["external"]["value"].get("path"), None);
    report("variants", &[result]);
}

#[test]
fn readonly_credentials_are_protected_without_hiding_ordinary_resource_query_parameters() {
    let mut tree = source_tree();
    tree["rule_set"][0] = yaml_serde::to_value(json!({
        "name":"remote-rules","type":"remote","format":"json",
        "url":"https://rules.example.test/list?format=compact",
    }))
    .unwrap();
    let fixture = Fixture::with_source(1, &encoded(&tree));
    tree["work"]["path"] = "./changed-work".into();
    tree["database"]["records_path"] = "./data/other-queries".into();
    tree["webui"]["port"] = 18081.into();
    tree["webui"]["users"] = yaml_serde::to_value(json!([{
        "name":"private-user","password_hash":"$argon2id$must-not-return-this-hash",
    }]))
    .unwrap();
    tree["rule_set"][0]["url"] = "https://rules.example.test/list?format=full".into();
    tree["upstreams"].as_sequence_mut().unwrap().push(yaml_serde::to_value(json!({
        "name":"query-upstream","type":"doh","address":"https://dns.example.test/dns-query?mode=standard",
        "connect_ip":"192.0.2.5",
    })).unwrap());
    tree["logs"]["level"] = "error".into();
    let result = project(&fixture, &tree);
    assert!(result["parse_error"].is_null());
    assert_eq!(
        result["protected_changes"],
        json!(["work", "database", "webui", "protected_credentials"])
    );
    assert_eq!(result["editable"].as_array().unwrap().len(), 3);
    assert_eq!(
        result["editable"][0]["external"]["value"]["address"],
        "https://dns.example.test/dns-query?mode=standard"
    );
    assert_eq!(
        result["editable"][1]["active"]["value"]["url"],
        "https://rules.example.test/list?format=compact"
    );
    assert_eq!(
        result["editable"][1]["external"]["value"]["url"],
        "https://rules.example.test/list?format=full"
    );
    assert_eq!(result["editable"][2]["external"]["module"], "logs");
    let output = result.to_string();
    for secret in [
        "private-user",
        "must-not-return",
        "changed-work",
        "other-queries",
    ] {
        assert!(!output.contains(secret), "{secret}");
    }
    report("protected", &[result]);
}

#[test]
fn names_are_matched_per_namespace_without_inferred_rename_or_delete_commands() {
    let mut tree = source_tree();
    let fixture = Fixture::with_source(1, &encoded(&tree));
    tree["rule_set"][0]["name"] = "renamed-rules".into();
    tree["upstreams"][0]["hosts"] = "127.0.0.2 localhost".into();
    tree["clients"][0]["client_id"] = "Other-Id".into();
    let result = project(&fixture, &tree);
    let changes = result["editable"].as_array().unwrap();
    assert_eq!(changes.len(), 4);
    assert_eq!(changes[0]["external"]["module"], "upstreams");
    assert_eq!(changes[1]["active"]["value"]["name"], "remote-rules");
    assert!(changes[1]["external"].is_null());
    assert!(changes[2]["active"].is_null());
    assert_eq!(changes[2]["external"]["value"]["name"], "renamed-rules");
    assert_eq!(changes[3]["active"]["value"]["client_id"], "Desktop-01");
    assert_eq!(changes[3]["external"]["value"]["client_id"], "Other-Id");
    // 这是读取差异，不是可重放写命令；客户端更新入口继续拒绝 client_id。
    assert!(crate::management::contract::decode_candidate(&serde_json::to_vec(&json!({
        "expected":result["expected"],"discard_external_changes":true,"changes":[{
            "module":"clients","change":{"action":"update","original_name":"desktop","value":changes[3]["external"]["value"]},
        }],
    })).unwrap()).is_err());
    report("names", &[result]);
}

#[test]
fn equivalent_source_expression_and_derived_only_changes_do_not_invent_editable_values() {
    let mut tree = source_tree();
    let fixture = Fixture::with_source(1, &encoded(&tree));
    let initial = fixture.state();
    // 命名集合顺序和注释不携带候选语义；客户 CIDR 表达经既有 parser 规范化。
    tree["clients"][0]["match"]["ips"][0] = "192.0.2.10/32".into();
    let result = project(&fixture, &tree);
    assert_eq!(result["editable"], json!([]));
    assert_ne!(
        result["expected"]["observed_file_revision"],
        initial["observed_file_revision"]
    );
    fs::write(&fixture.derived, "private-database: not-a-second-source").unwrap();
    let derived = serde_json::to_value(external_diff(fixture.store()).unwrap()).unwrap();
    assert_eq!(derived["editable"], json!([]));
    assert!(derived["parse_error"].is_null());
    assert!(!derived.to_string().contains("not-a-second-source"));
    report("equivalent", &[result, derived]);
}

#[test]
fn invalid_external_config_returns_only_a_bound_safe_error_and_never_partial_changes() {
    let fixture = Fixture::new(1);
    let initial = fixture.state();
    let mut samples = Vec::new();
    for (source, code) in [
        (
            "password: do-not-return\ninvalid: [".to_owned(),
            "VALIDATION_FAILED",
        ),
        (
            SOURCE.replace("version: 2", "version: 1"),
            "VERSION_UNSUPPORTED",
        ),
        (
            SOURCE.replace(
                "default_upstream: local",
                "default_upstream: missing-reference",
            ),
            "VALIDATION_FAILED",
        ),
        (
            SOURCE.replace(
                "records_path: ./data/queries",
                "records_path: ./data/statistics.sqlite3",
            ),
            "VALIDATION_FAILED",
        ),
        (
            SOURCE.replace(
                "reference_size_bytes: 1073741824",
                "reference_size_bytes: 18446744073709551615",
            ),
            "VALIDATION_FAILED",
        ),
        (SOURCE.replace("dns: {}", "dns: null"), "VALIDATION_FAILED"),
    ] {
        fs::write(&fixture.source, &source).unwrap();
        let result = serde_json::to_value(external_diff(fixture.store()).unwrap()).unwrap();
        assert_eq!(result["parse_error"], code);
        assert_eq!(result["editable"], json!([]));
        assert_eq!(result["protected_changes"], json!([]));
        assert_eq!(
            result["expected"]["active_revision"],
            initial["active_revision"]
        );
        assert!(!result.to_string().contains("do-not-return"));
        assert!(!result.to_string().contains("missing-reference"));
        samples.push(result);
    }
    fs::OpenOptions::new()
        .write(true)
        .open(&fixture.source)
        .unwrap()
        .set_len(MAX_CONFIG_BYTES as u64 + 1)
        .unwrap();
    let oversized = serde_json::to_value(external_diff(fixture.store()).unwrap()).unwrap();
    assert_eq!(oversized["parse_error"], "PAYLOAD_TOO_LARGE");
    fs::remove_file(&fixture.source).unwrap();
    let missing = serde_json::to_value(external_diff(fixture.store()).unwrap()).unwrap();
    assert_eq!(missing["parse_error"], "NOT_FOUND");
    fs::create_dir(&fixture.source).unwrap();
    let unreadable = serde_json::to_value(external_diff(fixture.store()).unwrap()).unwrap();
    assert_eq!(unreadable["parse_error"], "SERVICE_UNAVAILABLE");
    samples.extend([oversized, missing, unreadable]);
    report("invalid", &samples);
}

#[test]
fn diff_count_bytes_and_field_limits_reject_whole_response_without_truncation() {
    let fixture = Fixture::new(1);
    let mut tree = source_tree();
    for index in 0..126 {
        tree["hosts"].as_sequence_mut().unwrap().push(yaml_serde::to_value(json!({
            "name":format!("new-{index}"),"type":"const","format":"hosts","hosts":"127.0.0.1 localhost",
        })).unwrap());
    }
    fs::write(&fixture.source, encoded(&tree)).unwrap();
    assert_eq!(external_diff(fixture.store()).unwrap().editable.len(), 128);
    tree["hosts"].as_sequence_mut().unwrap().push(
        yaml_serde::to_value(json!({
            "name":"over-limit","type":"const","format":"hosts","hosts":"127.0.0.1 localhost",
        }))
        .unwrap(),
    );
    fs::write(&fixture.source, encoded(&tree)).unwrap();
    assert!(matches!(
        external_diff(fixture.store()),
        Err(ErrorCode::PayloadTooLarge)
    ));

    for (field, value) in [("logs", "a".repeat(4097)), ("outbound", "A".repeat(257))] {
        let mut tree = source_tree();
        if field == "logs" {
            tree["logs"]["path"] = value.into();
        } else {
            tree["outbound"][0]["proxy_url"]["env"] = value.into();
        }
        fs::write(&fixture.source, encoded(&tree)).unwrap();
        assert!(fixture.store().external_source().unwrap().external.is_ok());
        assert!(matches!(
            external_diff(fixture.store()),
            Err(ErrorCode::PayloadTooLarge)
        ));
    }
    let mut tree = source_tree();
    for index in 0..8 {
        tree["hosts"].as_sequence_mut().unwrap().push(yaml_serde::to_value(json!({
            "name":format!("large-{index}"),"type":"const","format":"hosts","hosts":"127.0.0.1 a\n".repeat(14000),
        })).unwrap());
    }
    let before = encoded(&tree);
    assert!(before.len() < MAX_CONFIG_BYTES);
    let fixture = Fixture::with_source(1, &before);
    for item in tree["hosts"].as_sequence_mut().unwrap().iter_mut().skip(1) {
        item["hosts"] = "127.0.0.2 b\n".repeat(14000).into();
    }
    let after = encoded(&tree);
    assert!(ConfigV2::parse(after.as_bytes()).is_ok());
    fs::write(&fixture.source, after).unwrap();
    assert!(matches!(
        external_diff(fixture.store()),
        Err(ErrorCode::PayloadTooLarge)
    ));
}
