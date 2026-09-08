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

#[test]
fn shared_http_and_ws_fixtures_round_trip_without_precision_loss() {
    let fixtures = fixtures();
    round_trip::<Candidate>(&fixtures["candidate"]);
    round_trip::<ConfigState>(&fixtures["state"]);
    round_trip::<QueryRequest>(&fixtures["query"]);
    round_trip::<QueryRecord>(&fixtures["record"]);
    round_trip::<ServiceMetrics>(&fixtures["metrics"]);
    round_trip::<WebSocketTicket>(&fixtures["websocket_ticket"]);
    round_trip::<ConfigRead>(&fixtures["config_read"]);
    round_trip::<ExternalDiff>(&fixtures["external_diff"]);
    round_trip::<ValidationResult>(&fixtures["validation"]);
    round_trip::<CacheSnapshotStatus>(&fixtures["snapshot"]);
    for value in fixtures["module_sources"].as_array().unwrap() {
        round_trip::<ModuleSource>(value);
    }
    for value in fixtures["operations"].as_array().unwrap() {
        round_trip::<OperationResult>(value);
    }
    for value in fixtures["client_messages"].as_array().unwrap() {
        round_trip::<ClientMessage>(value);
    }
    for value in fixtures["server_messages"].as_array().unwrap() {
        round_trip::<ServerMessage>(value);
    }
}

#[test]
fn mutation_shape_rejects_readonly_identity_delete_and_variant_residue() {
    let candidate = fixtures()["candidate"].clone();
    for change in [
        json!({"module":"database","change":{"path":"other.db"}}),
        json!({"module":"webui","change":{"users":[]}}),
        json!({"module":"work","change":{"path":"other"}}),
        json!({"module":"clients","change":{"action":"delete","original_name":"desktop"}}),
        json!({"module":"clients","change":{"action":"update","original_name":"desktop","value":{"name":"renamed","client_id":"altered"}}}),
        json!({"module":"listener","change":{"action":"create","value":{"type":"udp","name":"dns","addresses":["127.0.0.1"],"port":53,"strategy":"default","enable":true}}}),
        json!({"module":"listener","change":{"action":"create","value":{"type":"udp","name":"dns","addresses":["127.0.0.1"],"port":53,"strategy":"default","routes":[]}}}),
    ] {
        let mut value = candidate.clone();
        value["changes"] = json!([change]);
        assert!(decode_candidate(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut value = candidate;
    value["unexpected"] = json!(true);
    assert!(decode_candidate(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn request_budgets_and_tokens_fail_before_handler_work() {
    assert!(matches!(
        decode_candidate(&vec![b' '; MAX_MUTATION_BYTES + 1]),
        Err(ErrorCode::PayloadTooLarge)
    ));
    let mut value = fixtures()["candidate"].clone();
    value["changes"] = json!([]);
    assert!(decode_candidate(&serde_json::to_vec(&value).unwrap()).is_err());
    let change = fixtures()["candidate"]["changes"][0].clone();
    value["changes"] = Value::Array(vec![change; MAX_CHANGES + 1]);
    assert!(decode_candidate(&serde_json::to_vec(&value).unwrap()).is_err());
    for value in ["", "a/b", "a\n", &"a".repeat(129)] {
        assert!(serde_json::from_value::<Revision>(json!(value)).is_err());
    }
    assert!(serde_json::from_value::<Cursor>(json!("a".repeat(MAX_CURSOR_BYTES + 1))).is_err());
    assert!(decode_client_message(&vec![b' '; MAX_WS_FRAME_BYTES + 1]).is_err());
    let mut value = fixtures()["candidate"].clone();
    value["changes"] = json!([value["changes"][0]]);
    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(decode_module_candidate(&bytes, ConfigModule::Clients).is_ok());
    assert!(matches!(
        decode_module_candidate(&bytes, ConfigModule::Logs),
        Err(ErrorCode::Forbidden)
    ));
    value["changes"][0]["change"]["value"]["strategy"] = Value::Null;
    assert!(decode_candidate(&serde_json::to_vec(&value).unwrap()).is_err());
    for invalid in ["18446744073709551616", "01", "-1", "1e3", ""] {
        assert!(serde_json::from_value::<DecimalU64>(json!(invalid)).is_err());
    }
}

#[test]
fn apply_envelope_and_ws_filters_use_the_same_ingress_boundaries() {
    let mut apply = json!({
        "operation_id": "operation-1",
        "candidate": fixtures()["candidate"],
        "validation_token": "validation-1",
        "confirmations": []
    });
    assert!(decode_apply(&serde_json::to_vec(&apply).unwrap(), None).is_ok());
    assert!(matches!(
        decode_apply(
            &serde_json::to_vec(&apply).unwrap(),
            Some(ConfigModule::Clients)
        ),
        Err(ErrorCode::Forbidden)
    ));
    apply["confirmations"] = json!(["discard_external_changes", "discard_external_changes"]);
    assert!(decode_apply(&serde_json::to_vec(&apply).unwrap(), None).is_err());
    apply["confirmations"] = json!([]);
    apply["candidate"]["changes"][0]["change"]["value"]["strategy"] = Value::Null;
    assert!(decode_apply(&serde_json::to_vec(&apply).unwrap(), None).is_err());

    let mut file_sync = json!({
        "operation_id": "file-operation-1",
        "expected": {
            "active_revision": "active-1",
            "observed_file_revision": "files-1"
        },
        "discard_external_changes": true
    });
    assert!(decode_file_sync(&serde_json::to_vec(&file_sync).unwrap()).is_ok());
    file_sync["path"] = json!("not-accepted.yaml");
    assert!(decode_file_sync(&serde_json::to_vec(&file_sync).unwrap()).is_err());

    let base = json!({
        "type": "subscribe_queries",
        "subscription_id": "subscription-1",
        "filter": fixtures()["query"]["filter"],
        "after": {"epoch": "epoch-1", "sequence": "18446744073709551615"}
    });
    assert!(decode_client_message(&serde_json::to_vec(&base).unwrap()).is_ok());
    for (field, invalid) in [
        ("to_ms", json!(u64::MAX)),
        ("client_id", json!("invalid/id")),
        ("client_ip", json!("invalid")),
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
        ("client_ip", json!("not-an-ip")),
        ("qname", json!("a".repeat(254))),
    ] {
        let mut value = base.clone();
        value["filter"][field] = invalid;
        assert!(decode_query(&serde_json::to_vec(&value).unwrap()).is_err());
    }
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
    assert_eq!(value["value"]["cache"]["failure_ttl"], "5000000000ns");
    assert!(value["value"].get("resolve_log").is_none());
    let _: ModuleSource = serde_json::from_value(value).unwrap();
}

#[test]
fn operation_states_cannot_mix_success_and_failure_fields() {
    for value in [
        json!({"state":"unknown","active_revision":"active-1"}),
        json!({"state":"applied_synced","active_revision":"active-1"}),
        json!({"state":"applying","error":"APPLY_FAILED"}),
        json!({"state":"saved_pending_restart"}),
    ] {
        assert!(serde_json::from_value::<OperationStatus>(value).is_err());
    }
    assert!(
        serde_json::from_value::<HistoricalMatch>(
            json!({"source":"none","matched_client_id":"made-up"})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<HistoricalMatch>(json!({"source":"legacy_unknown"})).is_err());
}

#[test]
fn openapi_protection_constants_match_rust_and_only_p1_write_routes_are_registered() {
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
        ("operation_entries", MAX_OPERATION_ENTRIES),
        ("external_diff_bytes", MAX_EXTERNAL_DIFF_BYTES),
        (
            "access_token_ttl_seconds",
            crate::management::session::ACCESS_TOKEN_TTL.as_secs() as usize,
        ),
        (
            "access_renew_window_seconds",
            crate::management::session::ACCESS_RENEW_WINDOW.as_secs() as usize,
        ),
        ("operation_ttl_seconds", OPERATION_TTL_SECONDS as usize),
        ("websocket_protocol_version", WS_PROTOCOL_VERSION as usize),
    ] {
        assert_eq!(
            schema["x-limits"][key].as_u64(),
            Some(expected as u64),
            "{key}"
        );
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
    let routes = include_str!("../config_mutation.rs");
    assert!(routes.contains("/api/v2/config/apply"));
    assert!(routes.contains("/api/v2/config/files/restore"));
    assert!(routes.contains("/api/v2/config/modules/{module}/validate"));
    assert!(routes.contains("/api/v2/config/modules/{module}/apply"));
    let event_routes = include_str!("../events.rs");
    assert!(event_routes.contains("/api/v2/events/ticket"));
    assert!(event_routes.contains("/api/v2/events"));
}
