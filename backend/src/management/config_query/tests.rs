use super::*;
use crate::config::{
    edit::ConfigChange,
    model::{LogLevelDto, LogsDto},
    store::active::{ApplyPermit, BeginApply},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const SOURCE: &str = include_str!("../../../tests/fixtures/config-v2.yaml");

mod external;

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    derived: PathBuf,
    store: Option<Arc<ConfigStore>>,
}

impl Fixture {
    fn new(runtime_revision: u64) -> Self {
        Self::with_source(runtime_revision, SOURCE)
    }

    fn with_source(runtime_revision: u64, source_text: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/p1-config-query-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.yaml");
        let derived = root.join("config.yaml");
        let content = format!("{source_text}\n# private-configuration-sentinel\n");
        fs::write(&source, &content).unwrap();
        fs::write(&derived, &content).unwrap();
        let store = Arc::new(
            ConfigStore::with_active_source(source.clone(), &content, runtime_revision).unwrap(),
        );
        Self {
            root,
            source,
            derived,
            store: Some(store),
        }
    }

    fn store(&self) -> &Arc<ConfigStore> {
        self.store.as_ref().unwrap()
    }

    fn permit(&self, id: &str, level: LogLevelDto) -> ApplyPermit {
        let changes = vec![ConfigChange::Logs(LogsDto {
            enable: false,
            level,
            path: "./logs/fluxdns.log".into(),
        })];
        let BeginApply::Accepted(permit) = self
            .store()
            .begin_apply("actor", id, &changes, None, &Default::default())
            .unwrap()
        else {
            panic!("expected new operation");
        };
        *permit
    }

    fn state(&self) -> Value {
        serde_json::to_value(configuration_state(self.store()).unwrap()).unwrap()
    }

    fn result(&self, id: &str) -> Value {
        serde_json::to_value(operation_result(self.store(), "actor", id).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.store.take());
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/p1-config-query-tests")
            .canonicalize()
            .unwrap();
        let root = self.root.canonicalize().unwrap();
        assert_eq!(root.parent(), Some(base.as_path()));
        fs::remove_dir_all(root).unwrap();
    }
}

fn write_samples(name: &str, states: &[Value], operations: &[Value]) {
    // 报告只取白名单投影，供本机 schema 验证，不把动态 revision 或配置原文写入 Git。
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("_fluxdns/p1-config-query-projections");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(format!("{name}.json")),
        serde_json::to_vec(&json!({"states": states, "operations": operations})).unwrap(),
    )
    .unwrap();
}

#[test]
fn state_uses_cached_observation_and_decimal_runtime_revision_without_source() {
    let fixture = Fixture::new(u64::MAX);
    let initial = fixture.state();
    assert_eq!(initial["runtime_revision"], u64::MAX.to_string());
    assert_eq!(initial["synchronization"], "synced");
    assert_eq!(
        initial["files"],
        json!({"source":"unchanged","derived":"unchanged"})
    );
    assert!(
        !initial
            .to_string()
            .contains("private-configuration-sentinel")
    );
    assert_eq!(initial.as_object().unwrap().len(), 7);
    fs::write(&fixture.source, "password: actual-secret").unwrap();
    // GET 不隐式读文件，也不加载其配置；观测由明确的后台读取更新。
    assert_eq!(fixture.state(), initial);
    fixture.store().observe_files().unwrap();
    let external = fixture.state();
    assert_eq!(external["active_revision"], initial["active_revision"]);
    assert_eq!(external["runtime_revision"], initial["runtime_revision"]);
    assert_eq!(external["files"]["source"], "changed");
    assert!(!external.to_string().contains("actual-secret"));
    fs::remove_file(&fixture.derived).unwrap();
    fixture.store().observe_files().unwrap();
    assert_eq!(fixture.state()["files"]["derived"], "missing");
    let missing = fixture.state();
    fs::OpenOptions::new()
        .write(true)
        .open(&fixture.source)
        .unwrap()
        .set_len(crate::config::contract::MAX_CONFIG_BYTES as u64 + 1)
        .unwrap();
    fs::create_dir(&fixture.derived).unwrap();
    fixture.store().observe_files().unwrap();
    assert_eq!(
        fixture.state()["files"],
        json!({"source":"oversized","derived":"unreadable"})
    );

    write_samples(
        "files",
        &[initial, external, missing, fixture.state()],
        &[fixture.result("missing")],
    );
}

#[test]
fn completed_operation_keeps_its_revisions_after_later_applications_and_external_changes() {
    let fixture = Fixture::new(1);
    let mut first = fixture.permit("first", LogLevelDto::Warn);
    let mut operations = vec![fixture.result("first")];
    let mut states = vec![fixture.state()];
    assert_eq!(fixture.result("first")["status"]["state"], "preparing");
    assert_eq!(fixture.state()["synchronization"], "applying");
    first.begin_runtime_apply().unwrap();
    operations.push(fixture.result("first"));
    assert_eq!(fixture.result("first")["status"]["state"], "applying");
    first.applied(2).unwrap();
    operations.push(fixture.result("first"));
    states.push(fixture.state());
    assert_eq!(fixture.result("first")["status"]["state"], "persisting");
    assert_eq!(fixture.state()["synchronization"], "persisting");
    fixture.store().persist_applied("actor", "first").unwrap();
    let completed = fixture.result("first");
    operations.push(completed.clone());
    states.push(fixture.state());
    assert_eq!(completed["status"]["state"], "applied_synced");

    let mut next = fixture.permit("next", LogLevelDto::Error);
    next.begin_runtime_apply().unwrap();
    next.applied(3).unwrap();
    fixture.store().persist_applied("actor", "next").unwrap();
    assert_ne!(
        fixture.state()["active_revision"],
        completed["status"]["active_revision"]
    );
    fs::write(&fixture.source, "invalid: [").unwrap();
    fixture.store().observe_files().unwrap();
    assert_eq!(fixture.result("first"), completed);
    fixture.store().persist_applied("actor", "first").unwrap();
    assert_eq!(fixture.result("first"), completed);
    assert_eq!(fs::read_to_string(&fixture.source).unwrap(), "invalid: [");
    let other = operation_result(fixture.store(), "another-actor", "first").unwrap();
    assert!(matches!(other.status, OperationStatus::Unknown {}));
    assert!(matches!(
        operation_result(fixture.store(), "actor", "../bad"),
        Err(ErrorCode::InvalidArgument)
    ));
    write_samples("completed", &states, &operations);
}

#[test]
fn rejection_unknown_and_compensation_failure_do_not_invent_an_active_revision() {
    let fixture = Fixture::new(1);
    let rejected = fixture.permit("rejected", LogLevelDto::Warn);
    rejected
        .reject_with(OperationFailure::ValidationFailed, true)
        .unwrap();
    let mut operations = vec![fixture.result("rejected")];
    let mut states = vec![fixture.state()];
    assert_eq!(
        fixture.result("rejected")["status"],
        json!({"state":"rejected","error":"VALIDATION_FAILED"})
    );
    let abandoned = fixture.permit("unknown", LogLevelDto::Error);
    drop(abandoned);
    operations.push(fixture.result("unknown"));
    states.push(fixture.state());
    assert_eq!(
        fixture.result("unknown")["status"],
        json!({"state":"unknown"})
    );
    assert_eq!(fixture.state()["synchronization"], "blocked");

    let fixture = Fixture::new(1);
    fixture
        .permit("compensation", LogLevelDto::Warn)
        .rejected(false)
        .unwrap();
    operations.push(fixture.result("compensation"));
    states.push(fixture.state());
    assert_eq!(
        fixture.result("compensation")["status"],
        json!({
            "state":"compensation_failed","active_revision":null,"error":"COMPENSATION_FAILED",
        })
    );
    assert_eq!(fixture.state()["synchronization"], "blocked");
    write_samples("rejected", &states, &operations);
}

#[cfg(windows)]
#[test]
fn partial_self_write_is_known_but_external_same_content_identity_is_not() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let fixture = Fixture::new(1);
    let before = fixture.state();
    let mut permit = fixture.permit("partial", LogLevelDto::Warn);
    permit.begin_runtime_apply().unwrap();
    permit.applied(2).unwrap();
    let occupied = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&fixture.derived)
        .unwrap();
    assert!(fixture.store().persist_applied("actor", "partial").is_err());
    assert_eq!(fixture.state()["synchronization"], "applied_unpersisted");
    assert_eq!(
        fixture.state()["files"],
        json!({"source":"unchanged","derived":"unchanged"})
    );
    let failure = fixture.result("partial");
    let failed_state = fixture.state();
    assert_eq!(failure["status"]["error"], "PERSISTENCE_FAILED");
    assert_eq!(
        failure["status"]["persisted_revision"],
        before["persisted_revision"]
    );
    drop(occupied);
    fixture
        .store()
        .retry_persistence(
            "actor",
            "partial",
            &fixture.store().observe_files().unwrap().expected(),
            false,
        )
        .unwrap();
    assert_eq!(fixture.state()["synchronization"], "synced");
    assert_eq!(
        fixture.result("partial")["status"]["state"],
        "applied_synced"
    );

    let replacement = fixture.root.join("replacement.yaml");
    fs::copy(&fixture.source, &replacement).unwrap();
    fs::remove_file(&fixture.source).unwrap();
    fs::rename(&replacement, &fixture.source).unwrap();
    fixture.store().observe_files().unwrap();
    assert_eq!(fixture.state()["files"]["source"], "changed");
    assert_eq!(fixture.state()["files"]["derived"], "unchanged");
    write_samples(
        "partial",
        &[failed_state, fixture.state()],
        &[failure, fixture.result("partial")],
    );
}

#[tokio::test]
async fn remote_dat_limit_failure_reaches_operation_query_without_source_secrets() {
    use crate::management::{
        config_mutation::ConfigMutationOwner,
        contract::{ApplyRequest, OperationStatus},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let source = SOURCE.replace(
        "  rules_path: ./rules",
        "  rules_path: ./rules\n  rule_set_max_rules: 1",
    );
    let fixture = Fixture::with_source(1, &source);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // 两条 full 域名的 GeoSite protobuf；限制 1，且 strategy 引用 `cn`，确保该 selector
    // 经过真正下载和 DAT 解析后因实际加载的规则数超限而拒绝。
    let mut site = vec![0x0a, 2, b'C', b'N'];
    for name in ["a.test", "b.test"] {
        let mut domain = vec![0x08, 3, 0x12, name.len() as u8];
        domain.extend_from_slice(name.as_bytes());
        site.extend_from_slice(&[0x12, domain.len() as u8]);
        site.extend(domain);
    }
    let mut body = vec![0x0a, site.len() as u8];
    body.extend(site);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let length = stream.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..length]).contains("private-token-sentinel"));
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(&body).await.unwrap();
    });
    let owner = ConfigMutationOwner::new(
        Arc::clone(fixture.store()),
        crate::service::ServiceControl::unavailable_for_test(),
    );
    let request: ApplyRequest = serde_json::from_value(json!({
        "operation_id":"diagnostic-dat-apply",
        "changes":[{"module":"rule_set","change":{"action":"create","value":{
            "name":"diagnostic-dat","type":"remote","format":"dat",
            "url":format!("http://{address}/geosite.dat?token=private-token-sentinel"),"auto_update":false
        }}},{"module":"strategy","change":{"action":"create","value":{
            "name":"diagnostic-dat-strategy",
            "rules":[{"rule_set":"diagnostic-dat:cn","upstream":"local"}],
            "default_upstream":"local"
        }}}], "confirmations":[]
    })).unwrap();
    let initial = owner
        .start_apply("actor".to_owned(), request)
        .await
        .unwrap();
    assert!(matches!(initial.status, OperationStatus::Preparing {}));
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let result =
                operation_result(fixture.store(), "actor", "diagnostic-dat-apply").unwrap();
            if matches!(result.status, OperationStatus::Rejected { .. }) {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.await.unwrap();
    let diagnostic = result.diagnostic.as_ref().unwrap();
    assert_eq!(diagnostic.code, "RULE_TOO_MANY_RULES");
    assert_eq!(diagnostic.stage, "resource_parse");
    assert_eq!(diagnostic.resource.as_deref(), Some("diagnostic-dat"));
    assert!(diagnostic.message.contains("1"));
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(!encoded.contains("private-token-sentinel"));
    assert!(!encoded.contains(&address.to_string()));
    assert_eq!(
        fixture.store().active_snapshot().unwrap().runtime_revision,
        1
    );
    assert_eq!(
        fs::read_to_string(&fixture.source).unwrap(),
        format!("{source}\n# private-configuration-sentinel\n")
    );
    let again = operation_result(fixture.store(), "actor", "diagnostic-dat-apply").unwrap();
    assert_eq!(again.diagnostic, result.diagnostic);
}
