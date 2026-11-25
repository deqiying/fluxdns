use super::*;
use crate::config::disk;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc};

const SOURCE: &str = include_str!("../../../tests/fixtures/config-v2.yaml");

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    store: Arc<ConfigStore>,
}

impl Fixture {
    fn new(runtime_revision: u64) -> Self {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let name = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../_fluxdns/p2-config-query-tests")
            .join(name);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.yaml");
        fs::write(
            &source,
            format!("{SOURCE}\n# private-configuration-sentinel\n"),
        )
        .unwrap();
        let store = Arc::new(ConfigStore::open_for_test_at(&source, runtime_revision));
        Self {
            root,
            source,
            store,
        }
    }

    fn state(&self) -> Value {
        serde_json::to_value(configuration_state(&self.store).unwrap()).unwrap()
    }

    /// 模拟 watcher 上报一次当前代次的稳定观测。
    fn observe(&self) {
        self.store
            .observe_disk(self.store.disk_generation(), disk::observe(&self.source));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn state_projects_digest_decimal_revision_and_disk_status_without_source() {
    let fixture = Fixture::new(u64::MAX);
    let initial = fixture.state();
    assert_eq!(initial.as_object().unwrap().len(), 3);
    assert_eq!(initial["runtime_revision"], u64::MAX.to_string());
    assert_eq!(initial["disk"], json!({"status": "in_sync"}));
    assert_eq!(initial["config_digest"], fixture.store.current().digest);
    assert!(
        !initial
            .to_string()
            .contains("private-configuration-sentinel")
    );

    // GET 不隐式读文件；磁盘状态只由 watcher 观测或写操作更新。
    fs::write(&fixture.source, "password: actual-secret").unwrap();
    assert_eq!(fixture.state(), initial);
    fixture.observe();
    let external = fixture.state();
    assert_eq!(external["config_digest"], initial["config_digest"]);
    assert_eq!(external["runtime_revision"], initial["runtime_revision"]);
    assert_eq!(external["disk"]["status"], "modified");
    assert!(!external.to_string().contains("actual-secret"));
}

#[test]
fn disk_status_covers_missing_unreadable_and_restored_file() {
    let fixture = Fixture::new(1);
    let original = fs::read(&fixture.source).unwrap();
    fs::remove_file(&fixture.source).unwrap();
    fixture.observe();
    assert_eq!(fixture.state()["disk"]["status"], "missing");
    fs::write(
        &fixture.source,
        vec![b'x'; crate::config::contract::MAX_CONFIG_BYTES + 1],
    )
    .unwrap();
    fixture.observe();
    assert_eq!(fixture.state()["disk"]["status"], "unreadable");
    fs::write(&fixture.source, &original).unwrap();
    fixture.observe();
    assert_eq!(fixture.state()["disk"]["status"], "in_sync");
}

#[test]
fn restart_hint_maps_only_known_process_owned_components() {
    let fixture = Fixture::new(1);
    fixture
        .store
        .set_document_state_for_test(1, vec!["dns.resolve_log", "webui", "database"]);
    let document = fixture.store.current();
    let projected = document
        .restart_required
        .iter()
        .filter_map(|component| RestartComponent::from_component(component))
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(projected).unwrap(),
        json!(["dns.resolve_log", "webui", "database"])
    );
}
