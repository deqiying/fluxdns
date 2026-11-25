use super::*;
use crate::config::contract::ConfigV2;
use serde_json::{Value, json};

fn fixtures() -> Value {
    serde_json::from_str(include_str!("../../../tests/fixtures/management-v2.json")).unwrap()
}

fn round_trip<T: DeserializeOwned + Serialize>(value: &Value) {
    let typed: T = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(&serde_json::to_value(typed).unwrap(), value);
}

fn save(value: &Value) -> Result<SaveRequest, MutationDecodeError> {
    decode_save(&serde_json::to_vec(value).unwrap())
}

#[test]
fn shared_http_and_ws_fixtures_round_trip_without_precision_loss() {
    let fixtures = fixtures();
    round_trip::<SaveRequest>(&fixtures["save_request"]);
    round_trip::<ConfigState>(&fixtures["state"]);
    round_trip::<SaveResult>(&fixtures["save_result"]);
    round_trip::<ReloadResult>(&fixtures["reload_result"]);
    round_trip::<PersistResult>(&fixtures["reload_result"]);
    round_trip::<SystemConfigRead>(&fixtures["system"]);
    round_trip::<QueryRequest>(&fixtures["query"]);
    round_trip::<QueryRecord>(&fixtures["record"]);
    round_trip::<ServiceMetrics>(&fixtures["metrics"]);
    round_trip::<ProcessMetrics>(&fixtures["runtime"]);
    let memory: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/memory-analysis-v2.json"
    ))
    .unwrap();
    round_trip::<MemoryAnalysis>(&memory);
    round_trip::<WebSocketTicket>(&fixtures["websocket_ticket"]);
    round_trip::<ConfigRead>(&fixtures["config_read"]);
    round_trip::<CacheSnapshotStatus>(&fixtures["snapshot"]);
    for value in fixtures["module_sources"].as_array().unwrap() {
        round_trip::<ModuleSource>(value);
    }
    for value in fixtures["client_messages"].as_array().unwrap() {
        round_trip::<ClientMessage>(value);
    }
    for value in fixtures["server_messages"].as_array().unwrap() {
        round_trip::<ServerMessage>(value);
    }
}

#[test]
fn config_state_has_only_digest_runtime_revision_and_disk_status() {
    let state = fixtures()["state"].clone();
    for (field, value) in [
        ("active_revision", json!("active-8")),
        ("persisted_revision", json!("persisted-8")),
        ("synchronization", json!("synced")),
        (
            "files",
            json!({"source": "unchanged", "derived": "unchanged"}),
        ),
    ] {
        let mut legacy = state.clone();
        legacy[field] = value;
        assert!(
            serde_json::from_value::<ConfigState>(legacy).is_err(),
            "{field}"
        );
    }
    for status in ["in_sync", "modified", "missing", "unreadable"] {
        let mut value = state.clone();
        value["disk"]["status"] = json!(status);
        round_trip::<ConfigState>(&value);
    }
    let mut unknown = state;
    unknown["disk"]["status"] = json!("oversized");
    assert!(serde_json::from_value::<ConfigState>(unknown).is_err());
}

#[test]
fn runtime_timezone_fields_are_required_and_fallback_is_explicit() {
    let runtime = fixtures()["runtime"].clone();
    for field in ["timezone", "timezone_fallback_reason"] {
        let mut missing = runtime.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<ProcessMetrics>(missing).is_err());
    }
    let mut fallback = runtime;
    fallback["timezone"] = json!("UTC");
    fallback["timezone_fallback_reason"] = json!("invalid_tz");
    round_trip::<ProcessMetrics>(&fallback);
}

#[test]
fn mutation_shape_rejects_readonly_identity_delete_and_variant_residue() {
    let request = fixtures()["save_request"].clone();
    for change in [
        json!({"module":"database","change":{"path":"other.db"}}),
        json!({"module":"webui","change":{"users":[]}}),
        json!({"module":"work","change":{"path":"other"}}),
        json!({"module":"clients","change":{"action":"delete","original_name":"desktop"}}),
        json!({"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"renamed","client_ids":["altered"]}}}),
        json!({"module":"listener","change":{"action":"create","value":{"type":"udp","name":"dns","addresses":["127.0.0.1"],"port":53,"strategy":"default","enable":true}}}),
        json!({"module":"listener","change":{"action":"create","value":{"type":"udp","name":"dns","addresses":["127.0.0.1"],"port":53,"strategy":"default","routes":[]}}}),
    ] {
        let mut value = request.clone();
        value["changes"] = json!([change]);
        assert!(save(&value).is_err());
    }
    let mut value = request;
    value["unexpected"] = json!(true);
    assert!(save(&value).is_err());
    // 客户端更新允许显式提供新的 client_id；格式与唯一性由候选校验负责。
    value.as_object_mut().unwrap().remove("unexpected");
    value["changes"] = json!([{"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"desktop","client_id":"altered"}}}]);
    assert!(save(&value).is_ok());
}

#[test]
fn request_budgets_and_tokens_fail_before_handler_work() {
    assert!(matches!(
        decode_save(&vec![b' '; MAX_MUTATION_BYTES + 1])
            .unwrap_err()
            .code,
        ErrorCode::PayloadTooLarge
    ));
    let mut value = fixtures()["save_request"].clone();
    value["changes"] = json!([]);
    assert!(save(&value).is_err());
    let change = fixtures()["save_request"]["changes"][0].clone();
    value["changes"] = Value::Array(vec![change; MAX_CHANGES + 1]);
    assert!(save(&value).is_err());
    for value in ["", "a/b", "a\n", &"a".repeat(129)] {
        assert!(serde_json::from_value::<Revision>(json!(value)).is_err());
    }
    assert!(serde_json::from_value::<Cursor>(json!("a".repeat(MAX_CURSOR_BYTES + 1))).is_err());
    assert!(decode_client_message(&vec![b' '; MAX_WS_FRAME_BYTES + 1]).is_err());
    let mut value = fixtures()["save_request"].clone();
    value["changes"] = json!([value["changes"][0]]);
    assert!(save(&value).is_ok());
    value["changes"][0]["change"]["value"]["strategy"] = Value::Null;
    assert!(save(&value).is_err());
    for invalid in ["18446744073709551616", "01", "-1", "1e3", ""] {
        assert!(serde_json::from_value::<DecimalU64>(json!(invalid)).is_err());
    }
}

#[test]
fn save_request_has_only_changes_and_two_explicit_flags() {
    let request = fixtures()["save_request"].clone();
    let mut minimal = json!({"changes": request["changes"]});
    let decoded = save(&minimal).unwrap();
    assert!(!decoded.overwrite_disk);
    assert!(!decoded.confirm_retention_shortening);
    minimal["overwrite_disk"] = json!(true);
    minimal["confirm_retention_shortening"] = json!(true);
    let decoded = save(&minimal).unwrap();
    assert!(decoded.overwrite_disk);
    assert!(decoded.confirm_retention_shortening);
    // 旧 apply 封套的 operation、确认与文件版本字段一律拒绝。
    for (field, value) in [
        ("operation_id", json!("operation-1")),
        ("confirmations", json!(["retention_shortening"])),
        ("observed_file_revision", json!("files-1")),
        ("active_revision", json!("stale")),
        (
            "expected",
            json!({"active_revision": "stale", "observed_file_revision": "stale"}),
        ),
        ("validation_token", json!("old-ticket")),
    ] {
        let mut legacy = request.clone();
        legacy[field] = value;
        assert!(save(&legacy).is_err(), "{field}");
    }
    let mut wrong_type = request;
    wrong_type["overwrite_disk"] = json!("yes");
    assert!(save(&wrong_type).is_err());
}

#[test]
fn invalid_rule_ecs_custom_ip_returns_only_its_dto_path() {
    for mode in ["custom", "client_or_custom"] {
        let request = json!({
            "changes": [{
                "module": "strategy",
                "change": {
                    "action": "update",
                    "original_name": "default",
                    "value": {
                        "name": "default",
                        "rules": [
                            {"hosts": "local"},
                            {"hosts": "local", "edns_client_subnet": {"mode": mode, "custom_ip": "192.0.2.0/99"}}
                        ],
                        "default_upstream": "public"
                    }
                }
            }]
        });
        let error = save(&request).unwrap_err();
        assert!(matches!(error.code, ErrorCode::InvalidArgument));
        assert_eq!(error.field_errors.len(), 1);
        assert_eq!(
            error.field_errors[0].path,
            "changes[0].change.value.rules[1].edns_client_subnet.custom_ip"
        );
        assert!(matches!(
            error.field_errors[0].code,
            ErrorCode::InvalidArgument
        ));
        assert!(!format!("{error:?}").contains("192.0.2.0/99"));
    }
}

#[test]
fn ws_filters_use_the_same_ingress_boundaries_as_queries() {
    let base = json!({
        "type": "subscribe_queries",
        "subscription_id": "subscription-1",
        "filter": fixtures()["query"]["filter"],
        "after": {"epoch": "epoch-1", "sequence": "18446744073709551615"},
        "retention_revision": "9"
    });
    assert!(decode_client_message(&serde_json::to_vec(&base).unwrap()).is_ok());
    for (field, invalid) in [
        ("to_ms", json!(u64::MAX)),
        ("client_id", json!("invalid/id")),
        ("client_ip", json!("a".repeat(65))),
        ("client_ip", json!(format!("bad{}ip", char::from(0)))),
        ("qname", json!("\nexample")),
        ("client_ip", json!("192.168\t")),
        ("client_name", json!(" ".repeat(129))),
        ("qname", json!("a".repeat(254))),
        ("qname", Value::Null),
    ] {
        let mut value = base.clone();
        value["filter"][field] = invalid;
        assert!(
            decode_client_message(&serde_json::to_vec(&value).unwrap()).is_err(),
            "{field}"
        );
        let mut query = fixtures()["query"].clone();
        query["filter"] = value["filter"].clone();
        assert!(
            decode_query(&serde_json::to_vec(&query).unwrap()).is_err(),
            "{field}"
        );
    }
    // 配置变化通知只携带配置摘要，不再有 active/observed 两个版本。
    assert!(
        serde_json::from_value::<ServerMessage>(json!({
            "type": "config_changed",
            "active_revision": "active-8",
            "observed_file_revision": "files-10"
        }))
        .is_err()
    );
}

#[test]
fn query_window_page_size_and_identity_bounds_are_checked() {
    let base = fixtures()["query"].clone();
    assert!(decode_query(&serde_json::to_vec(&base).unwrap()).is_ok());
    for (field, invalid) in [
        ("page_size", json!(0)),
        ("page_size", json!(101)),
        ("cursor", json!("a".repeat(2049))),
    ] {
        let mut value = base.clone();
        value[field] = invalid;
        assert!(decode_query(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    for (field, invalid) in [
        ("from_ms", base["filter"]["to_ms"].clone()),
        ("to_ms", json!(9007199254740992_u64)),
        ("client_id", json!("bad/id")),
        ("client_ip", json!("a".repeat(65))),
        ("qname", json!("a".repeat(254))),
    ] {
        let mut value = base.clone();
        value["filter"][field] = invalid;
        assert!(decode_query(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}

#[test]
fn query_text_filters_trim_lowercase_and_accept_ip_fragments() {
    let mut query = fixtures()["query"].clone();
    query["filter"]["qname"] = json!("  ExAmPlE.  ");
    query["filter"]["client_ip"] = json!("  2001:DB8  ");
    query["filter"]["client_name"] = json!("  DeskTop  ");
    let query = decode_query(&serde_json::to_vec(&query).unwrap()).unwrap();
    assert_eq!(query.filter.qname.as_deref(), Some("example."));
    assert_eq!(query.filter.client_ip.as_deref(), Some("2001:db8"));
    assert_eq!(query.filter.client_name.as_deref(), Some("desktop"));

    let mut message = fixtures()["client_messages"][1].clone();
    message["filter"]["qname"] = json!("   ");
    message["filter"]["client_ip"] = json!("  192.168  ");
    let ClientMessage::SubscribeQueries { filter, .. } =
        decode_client_message(&serde_json::to_vec(&message).unwrap()).unwrap()
    else {
        panic!("fixture must subscribe to queries");
    };
    assert_eq!(filter.qname, None);
    assert_eq!(filter.client_ip.as_deref(), Some("192.168"));
}
#[test]
fn shared_source_serialization_preserves_omission_paths_and_duration_shape() {
    let config = ConfigV2::parse(include_bytes!("../../../tests/fixtures/config-v2.yaml")).unwrap();
    let source = ModuleSource::Clients(config.clients[0].clone());
    let json = serde_json::to_value(&source).unwrap();
    assert_eq!(json["value"]["client_id"], "Desktop-01");
    assert!(json["value"].get("cache").is_none());
    assert!(json["value"].get("ttl_override").is_none());
    let value = serde_json::to_value(ModuleSource::Dns(DnsV2 {
        cache: Some(crate::config::contract::GlobalCacheV2::default()),
        ..DnsV2::default()
    }))
    .unwrap();
    assert_eq!(
        value["value"]["cache"]["persistence"]["path"],
        "./data/dns-cache.db"
    );
    assert_eq!(value["value"]["cache"]["failure_ttl"], "5s");
    assert!(value["value"].get("resolve_log").is_none());
    let _: ModuleSource = serde_json::from_value(value).unwrap();
}

#[test]
fn historical_match_states_cannot_mix_fields() {
    assert!(
        serde_json::from_value::<HistoricalMatch>(
            json!({"source":"none","matched_client_id":"made-up"})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<HistoricalMatch>(json!({"source":"legacy_unknown"})).is_err());
}

#[test]
fn openapi_protection_constants_match_rust_and_only_config_write_routes_are_registered() {
    let schema: yaml_serde::Value = yaml_serde::from_str(include_str!(
        "../../../../frontend/openapi/management-api-v2.yaml"
    ))
    .unwrap();
    assert_eq!(schema["servers"][0]["url"].as_str(), Some(API_PREFIX));
    for (key, expected) in [
        ("mutation_bytes", MAX_MUTATION_BYTES),
        ("candidate_changes", MAX_CHANGES),
        ("cursor_bytes", MAX_CURSOR_BYTES),
        ("query_body_bytes", MAX_QUERY_BYTES),
        ("websocket_frame_bytes", MAX_WS_FRAME_BYTES),
        ("websocket_connections", WS_CONNECTION_CAPACITY),
        (
            "websocket_connections_per_session",
            WS_CONNECTIONS_PER_SESSION,
        ),
        (
            "websocket_subscriptions_per_connection",
            WS_SUBSCRIPTIONS_PER_CONNECTION,
        ),
        ("websocket_queue_bytes", WS_QUEUE_BYTES),
        ("websocket_queue_messages", WS_QUEUE_MESSAGES),
        ("websocket_heartbeat_seconds", WS_HEARTBEAT_SECONDS as usize),
        ("websocket_idle_seconds", WS_IDLE_SECONDS as usize),
        (
            "websocket_write_timeout_seconds",
            WS_WRITE_TIMEOUT_SECONDS as usize,
        ),
        (
            "websocket_inbound_messages_per_minute",
            WS_INBOUND_MESSAGES_PER_MINUTE,
        ),
        (
            "websocket_ticket_ttl_seconds",
            crate::management::session::WS_TICKET_TTL.as_secs() as usize,
        ),
        (
            "websocket_ticket_entries",
            crate::management::session::WS_TICKET_GLOBAL_CAPACITY,
        ),
        (
            "websocket_tickets_per_session",
            crate::management::session::WS_TICKET_PER_SESSION_CAPACITY,
        ),
        ("online_identity_entries", MAX_ONLINE_IDENTITIES),
        ("replay_seconds", REPLAY_SECONDS as usize),
        ("replay_records", REPLAY_RECORDS),
        ("replay_bytes", REPLAY_BYTES),
        (
            "access_token_ttl_seconds",
            crate::management::session::ACCESS_TOKEN_TTL.as_secs() as usize,
        ),
        (
            "access_renew_window_seconds",
            crate::management::session::ACCESS_RENEW_WINDOW.as_secs() as usize,
        ),
        ("websocket_protocol_version", WS_PROTOCOL_VERSION as usize),
    ] {
        assert_eq!(
            schema["x-limits"][key].as_u64(),
            Some(expected as u64),
            "{key}"
        );
    }
    for removed in [
        "operation_entries",
        "operation_ttl_seconds",
        "external_diff_bytes",
    ] {
        assert!(schema["x-limits"].get(removed).is_none(), "{removed}");
    }
    for (code, status) in schema["x-error-statuses"].as_mapping().unwrap() {
        let code: ErrorCode = serde_json::from_value(json!(code.as_str().unwrap())).unwrap();
        assert_eq!(u64::from(code.http_status()), status.as_u64().unwrap());
    }
    assert_eq!(
        schema["x-error-statuses"].as_mapping().unwrap().len(),
        schema["components"]["schemas"]["ErrorCode"]["enum"]
            .as_sequence()
            .unwrap()
            .len()
    );
    let paths = schema["paths"].as_mapping().unwrap();
    for path in ["/config/save", "/config/reload", "/config/persist"] {
        assert!(paths.contains_key(path), "{path}");
    }
    for path in [
        "/config/validate",
        "/config/apply",
        "/config/files/restore",
        "/config/files/retry",
        "/config/external",
        "/config/operations/{operation_id}",
    ] {
        assert!(!paths.contains_key(path), "{path}");
    }
    let routes = include_str!("../config_api.rs");
    assert!(routes.contains("\"/api/v2/config/save\""));
    assert!(routes.contains("\"/api/v2/config/reload\""));
    assert!(routes.contains("\"/api/v2/config/persist\""));
    let event_routes = include_str!("../events.rs");
    assert!(event_routes.contains("/api/v2/events/ticket"));
    assert!(event_routes.contains("/api/v2/events"));
}

#[test]
fn retention_preview_does_not_accept_revision_preconditions() {
    let preview = json!({"policy": {"retention": {"days": 7, "grace_days": 3, "reference_size_bytes": 1073741824}}});
    assert!(serde_json::from_value::<RetentionPreviewRequest>(preview.clone()).is_ok());
    let mut legacy = preview;
    legacy["expected"] =
        json!({"active_revision": "active-1", "observed_file_revision": "files-1"});
    assert!(serde_json::from_value::<RetentionPreviewRequest>(legacy).is_err());
}

#[test]
fn ecs_contract_states_are_strict_and_legacy_defaults_to_null() {
    let fixture = fixtures();
    for ecs in fixture["ecs_cases"].as_array().unwrap() {
        round_trip::<QueryEcs>(ecs);
        let mut record = fixture["record"].clone();
        record["ecs"] = ecs.clone();
        round_trip::<QueryRecord>(&record);
        round_trip::<QueryDetail>(&json!({"record": record, "directory_revision": "directory-1"}));
        let value: QueryEcs = serde_json::from_value(ecs.clone()).unwrap();
        let debug = format!("{value:?}");
        assert!(!debug.contains("2001:db8"));
        assert!(!debug.contains("203.0.113"));
        assert!(!debug.contains("refresh-leaf"));
    }
    for ecs in fixture["ecs_invalid_cases"].as_array().unwrap() {
        assert!(serde_json::from_value::<QueryEcs>(ecs.clone()).is_err());
    }
    let mut legacy = fixture["record"].clone();
    legacy.as_object_mut().unwrap().remove("ecs");
    let typed: QueryRecord = serde_json::from_value(legacy).unwrap();
    assert!(typed.ecs.is_none());
    assert_eq!(serde_json::to_value(typed).unwrap()["ecs"], Value::Null);
    let mut invalid = fixture["ecs_cases"][0].clone();
    invalid["request"]["cidr"] = json!("2001:db8::1/32");
    assert!(serde_json::from_value::<QueryEcs>(invalid).is_err());
    for field in ["request", "upstream", "cache_match", "refresh"] {
        let mut invalid = fixture["ecs_cases"][0].clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<QueryEcs>(invalid).is_err(),
            "{field}"
        );
    }
}
