use std::fs;
use std::sync::Mutex as StdMutex;

use tokio::sync::{Notify, Semaphore};

use super::*;
use crate::config::contract::ResolveLogV2;
use crate::config::model::LogLevelDto;

const PASSWORD_HASH: &str = "$2b$04$QEeYuMZftq59wD41AcT2ruVzQinG4azJldvj/LXO7u9bzmP6dX6ri";

/// 不含凭据引用的最小配置；首行注释用于验证条目级写回保留未编辑内容。
fn source_text(work: &str) -> String {
    format!(
        r#"# 配置存储测试：未编辑段的注释必须保留
version: 2
work:
  path: {work}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
  records_path: ./queries
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {{}}
statistics:
  retention:
    days: 7
    grace_days: 3
    reference_size_bytes: 1073741824
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 15353
    strategy: default
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "127.0.0.1 example.test"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: "127.0.0.1 example.test"
outbound: []
rule_set: []
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
clients: []
"#
    )
}

#[derive(Clone, Copy)]
enum Mode {
    Apply,
    Fail,
    /// Runtime 已切换但回执丢失。
    LostApplied,
    /// 回执丢失且 Runtime 未切换。
    LostNotApplied,
}

/// Runtime 替身：记录代次与配置，可注入失败、回执丢失、阻塞与应用期间的外部写盘。
struct FakeHost {
    state: StdMutex<(u64, Arc<ResolvedConfig>)>,
    mode: StdMutex<Mode>,
    during_apply: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    entered: Notify,
    gate: Option<Semaphore>,
}

impl FakeHost {
    fn new(store: &ConfigStore, source: &Path, gated: bool) -> Self {
        let document = store.current();
        let resolved = resolve_config_v2(
            &document.config,
            deterministic_hash(document.text.as_bytes()),
            source,
        )
        .unwrap()
        .resolved;
        Self {
            state: StdMutex::new((document.runtime_revision, resolved)),
            mode: StdMutex::new(Mode::Apply),
            during_apply: StdMutex::new(None),
            entered: Notify::new(),
            gate: gated.then(|| Semaphore::new(0)),
        }
    }

    fn set_mode(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn revision(&self) -> u64 {
        self.state.lock().unwrap().0
    }
}

impl RuntimeHost for FakeHost {
    fn current(&self) -> (u64, Arc<ResolvedConfig>) {
        self.state.lock().unwrap().clone()
    }

    async fn apply(&self, expected: u64, config: Arc<ResolvedConfig>) -> ApplyOutcome {
        self.entered.notify_one();
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        if let Some(hook) = self.during_apply.lock().unwrap().take() {
            hook();
        }
        let mode = *self.mode.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        assert_eq!(state.0, expected);
        match mode {
            Mode::Apply => {
                *state = (expected + 1, config);
                ApplyOutcome::Applied(expected + 1)
            }
            Mode::Fail => ApplyOutcome::Failed(Diagnostic::new(
                "runtime_prepare",
                "TEST_PREPARE_FAILED",
                "测试注入的准备失败",
            )),
            Mode::LostApplied => {
                *state = (expected + 1, config);
                ApplyOutcome::Unknown
            }
            Mode::LostNotApplied => ApplyOutcome::Unknown,
        }
    }
}

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    mirror: PathBuf,
    original: String,
    store: Arc<ConfigStore>,
    host: Arc<FakeHost>,
}

impl Fixture {
    fn new() -> Self {
        Self::build(false)
    }

    fn gated() -> Self {
        Self::build(true)
    }

    fn build(gated: bool) -> Self {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let name = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p2-config-store");
        fs::create_dir_all(&base).unwrap();
        let root = fs::canonicalize(base).unwrap().join(name);
        fs::create_dir_all(&root).unwrap();
        // 生产启动时工作目录已由快照创建；镜像写入不负责建目录。
        fs::create_dir_all(root.join("work")).unwrap();
        let work = root.join("work").to_string_lossy().replace('\\', "/");
        let original = source_text(&work);
        let source = root.join("source.yaml");
        fs::write(&source, &original).unwrap();
        let store = Arc::new(ConfigStore::open_for_test(&source));
        let host = Arc::new(FakeHost::new(&store, &store.source_path, gated));
        Self {
            mirror: root.join("work/config.yaml"),
            root,
            source,
            original,
            store,
            host,
        }
    }

    fn disk_status(&self) -> DiskStatus {
        self.store.disk_status(&self.store.current().digest)
    }

    fn file(&self) -> String {
        fs::read_to_string(&self.source).unwrap()
    }

    async fn save(&self, input: SaveInput) -> Result<SaveOutcome, StoreError> {
        self.store.save(self.host.as_ref(), input).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        set_readonly(&self.source, false);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn set_readonly(path: &Path, readonly: bool) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(readonly);
        fs::set_permissions(path, permissions).unwrap();
    }
}

fn input(changes: Vec<ConfigChange>) -> SaveInput {
    SaveInput {
        changes,
        overwrite_disk: false,
        confirm_retention_shortening: false,
    }
}

/// 切换日志级别：不涉及启动级字段的普通变更。
fn logs_change(store: &ConfigStore) -> ConfigChange {
    let mut logs = store.current().config.logs.clone();
    logs.level = if matches!(logs.level, LogLevelDto::Warn) {
        LogLevelDto::Error
    } else {
        LogLevelDto::Warn
    };
    ConfigChange::Logs(logs)
}

#[tokio::test]
async fn save_applies_publishes_and_persists_source_and_mirror() {
    let fixture = Fixture::new();
    let before = fixture.store.current();
    let outcome = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .unwrap();
    assert!(outcome.persisted);
    assert!(outcome.diagnostic.is_none());
    assert_eq!(outcome.document.runtime_revision, 2);
    assert_eq!(fixture.host.revision(), 2);
    assert!(Arc::ptr_eq(&outcome.document, &fixture.store.current()));
    assert_ne!(outcome.document.digest, before.digest);
    assert_eq!(fixture.file(), &*outcome.document.text);
    assert_eq!(
        fs::read_to_string(&fixture.mirror).unwrap(),
        &*outcome.document.text
    );
    assert_eq!(fixture.disk_status(), DiskStatus::InSync);
    // 条目级写回：未编辑段的注释保持不变。
    let first_comment = fixture
        .original
        .lines()
        .find(|line| line.trim_start().starts_with('#'))
        .unwrap();
    assert!(outcome.document.text.contains(first_comment));
    assert!(outcome.document.restart_required.is_empty());
}

#[tokio::test]
async fn save_refuses_changed_or_missing_disk_unless_overwrite() {
    let fixture = Fixture::new();
    let external = format!("{}\n# external edit\n", fixture.original);
    fs::write(&fixture.source, &external).unwrap();
    let error = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error,
        StoreError::FileChanged(DiskStatus::Modified)
    ));
    assert_eq!(fixture.host.revision(), 1);
    assert_eq!(fixture.store.current().runtime_revision, 1);
    assert_eq!(fixture.file(), external);
    assert_eq!(fixture.disk_status(), DiskStatus::Modified);

    fs::remove_file(&fixture.source).unwrap();
    let error = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error,
        StoreError::FileChanged(DiskStatus::Missing)
    ));

    let outcome = fixture
        .save(SaveInput {
            overwrite_disk: true,
            ..input(vec![logs_change(&fixture.store)])
        })
        .await
        .unwrap();
    assert!(outcome.persisted);
    assert_eq!(fixture.file(), &*outcome.document.text);
    assert_eq!(fixture.disk_status(), DiskStatus::InSync);
}

#[tokio::test]
async fn concurrent_writes_are_rejected_as_busy() {
    let fixture = Fixture::gated();
    let store = Arc::clone(&fixture.store);
    let host = Arc::clone(&fixture.host);
    let change = logs_change(&store);
    let entered = fixture.host.entered.notified();
    let first = tokio::spawn(async move { store.save(host.as_ref(), input(vec![change])).await });
    entered.await;
    assert!(matches!(
        fixture.save(input(vec![logs_change(&fixture.store)])).await,
        Err(StoreError::Busy)
    ));
    assert!(matches!(
        fixture.store.reload(fixture.host.as_ref()).await,
        Err(StoreError::Busy)
    ));
    assert!(matches!(
        fixture.store.persist().await,
        Err(StoreError::Busy)
    ));
    assert!(matches!(
        fixture
            .store
            .create_initial_user("admin", PASSWORD_HASH)
            .await,
        Err(StoreError::Busy)
    ));
    fixture.host.gate.as_ref().unwrap().add_permits(1);
    assert!(first.await.unwrap().unwrap().persisted);
}

#[tokio::test]
async fn retention_shortening_requires_confirmation() {
    let fixture = Fixture::new();
    let mut statistics = fixture.store.current().config.statistics.clone();
    assert!(statistics.retention.days >= 2);
    statistics.retention.days -= 1;
    let change = ConfigChange::Statistics(statistics);
    let error = fixture
        .save(input(vec![change.clone()]))
        .await
        .err()
        .unwrap();
    assert!(matches!(error, StoreError::ConfirmationRequired));
    assert_eq!(fixture.host.revision(), 1);
    let outcome = fixture
        .save(SaveInput {
            confirm_retention_shortening: true,
            ..input(vec![change])
        })
        .await
        .unwrap();
    assert!(outcome.persisted);
}

#[tokio::test]
async fn apply_failure_keeps_document_runtime_and_file() {
    let fixture = Fixture::new();
    let before = fixture.store.current();
    fixture.host.set_mode(Mode::Fail);
    let error = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .err()
        .unwrap();
    let StoreError::Apply(diagnostic) = error else {
        panic!("expected apply failure");
    };
    assert_eq!(diagnostic.code, "TEST_PREPARE_FAILED");
    assert!(Arc::ptr_eq(&before, &fixture.store.current()));
    assert_eq!(fixture.host.revision(), 1);
    assert_eq!(fixture.file(), fixture.original);
}

#[tokio::test]
async fn lost_receipt_is_resolved_by_runtime_revision() {
    let fixture = Fixture::new();
    fixture.host.set_mode(Mode::LostNotApplied);
    let error = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, StoreError::Apply(ref diagnostic) if diagnostic.code == "RUNTIME_OUTCOME_UNKNOWN")
    );
    assert_eq!(fixture.store.current().runtime_revision, 1);

    fixture.host.set_mode(Mode::LostApplied);
    let outcome = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .unwrap();
    assert_eq!(outcome.document.runtime_revision, 2);
    assert!(outcome.persisted);
}

#[tokio::test]
async fn write_failure_keeps_runtime_and_reports_modified_until_persist() {
    let fixture = Fixture::new();
    set_readonly(&fixture.source, true);
    let outcome = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .unwrap();
    assert!(!outcome.persisted);
    assert_eq!(
        outcome.diagnostic.as_ref().unwrap().code,
        "CONFIG_FILE_WRITE_FAILED"
    );
    // 先应用后写盘（I7）：写盘失败不回滚 Runtime。
    assert_eq!(fixture.host.revision(), 2);
    assert!(Arc::ptr_eq(&outcome.document, &fixture.store.current()));
    assert_eq!(fixture.file(), fixture.original);
    assert_eq!(fixture.disk_status(), DiskStatus::Modified);

    let error = fixture.store.persist().await.err().unwrap();
    assert!(matches!(error, StoreError::Persist(_)));
    set_readonly(&fixture.source, false);
    let document = fixture.store.persist().await.unwrap();
    assert_eq!(fixture.file(), &*document.text);
    assert_eq!(fixture.disk_status(), DiskStatus::InSync);
    assert_eq!(fixture.host.revision(), 2);
}

#[tokio::test]
async fn external_edit_during_save_is_not_overwritten() {
    let fixture = Fixture::new();
    let external = format!("{}\n# edited during save\n", fixture.original);
    let source = fixture.source.clone();
    let written = external.clone();
    *fixture.host.during_apply.lock().unwrap() =
        Some(Box::new(move || fs::write(source, written).unwrap()));
    let outcome = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .unwrap();
    assert!(!outcome.persisted);
    assert_eq!(
        outcome.diagnostic.as_ref().unwrap().code,
        "CONFIG_FILE_CHANGED_DURING_SAVE"
    );
    assert_eq!(fixture.file(), external);
    assert_eq!(fixture.host.revision(), 2);
    assert_eq!(fixture.disk_status(), DiskStatus::Modified);
}

#[tokio::test]
async fn mirror_failure_only_adds_diagnostic() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.mirror).unwrap();
    let outcome = fixture
        .save(input(vec![logs_change(&fixture.store)]))
        .await
        .unwrap();
    assert!(outcome.persisted);
    assert_eq!(
        outcome.diagnostic.as_ref().unwrap().code,
        "CONFIG_MIRROR_WRITE_FAILED"
    );
    assert_eq!(fixture.file(), &*outcome.document.text);
    assert_eq!(fixture.disk_status(), DiskStatus::InSync);
}

#[tokio::test]
async fn process_owned_change_is_saved_but_runtime_keeps_startup_value() {
    let fixture = Fixture::new();
    let running = fixture.host.current().1.dns.resolve_log.enable;
    let mut dns = fixture.store.current().config.dns.clone();
    dns.resolve_log = Some(ResolveLogV2 { enable: !running });
    let outcome = fixture
        .save(input(vec![ConfigChange::Dns(dns)]))
        .await
        .unwrap();
    assert!(outcome.persisted);
    assert_eq!(outcome.document.restart_required, vec!["dns.resolve_log"]);
    assert_eq!(
        outcome
            .document
            .config
            .dns
            .resolve_log
            .as_ref()
            .unwrap()
            .enable,
        !running
    );
    // Runtime 换代但启动级字段沿用当前值，避免 service 拒绝切换。
    assert_eq!(fixture.host.revision(), 2);
    assert_eq!(fixture.host.current().1.dns.resolve_log.enable, running);

    // 改回启动值后不再需要重启。
    let mut dns = fixture.store.current().config.dns.clone();
    dns.resolve_log = Some(ResolveLogV2 { enable: running });
    let outcome = fixture
        .save(input(vec![ConfigChange::Dns(dns)]))
        .await
        .unwrap();
    assert!(outcome.document.restart_required.is_empty());
    assert_eq!(fixture.host.revision(), 3);

    // 外部修改启动级字段后 reload 同样成功，并给出重启提示。
    let text = fixture.file();
    let edited = if text.contains("resolve_log") {
        text.replace(
            "resolve_log:\n    enable: false",
            "resolve_log:\n    enable: true",
        )
    } else {
        text.replace("dns: {}", "dns:\n  resolve_log:\n    enable: true")
    };
    assert_ne!(edited, text);
    fs::write(&fixture.source, &edited).unwrap();
    let document = fixture.store.reload(fixture.host.as_ref()).await.unwrap();
    assert_eq!(document.restart_required, vec!["dns.resolve_log"]);
    assert_eq!(fixture.host.current().1.dns.resolve_log.enable, running);
}

#[tokio::test]
async fn reload_applies_disk_text_or_reports_unreadable() {
    let fixture = Fixture::new();
    let before = fixture.store.current();
    let same = fixture.store.reload(fixture.host.as_ref()).await.unwrap();
    assert!(Arc::ptr_eq(&before, &same));
    assert_eq!(fixture.host.revision(), 1);

    fs::write(&fixture.source, "invalid: [yaml").unwrap();
    assert!(matches!(
        fixture.store.reload(fixture.host.as_ref()).await,
        Err(StoreError::Edit(EditError::Validation(_)))
    ));
    assert!(Arc::ptr_eq(&before, &fixture.store.current()));
    assert_eq!(fixture.disk_status(), DiskStatus::Modified);

    fs::remove_file(&fixture.source).unwrap();
    assert!(matches!(
        fixture.store.reload(fixture.host.as_ref()).await,
        Err(StoreError::FileUnreadable(DiskStatus::Missing))
    ));

    let external = format!("{}\n# reloaded\n", fixture.original);
    fs::write(&fixture.source, &external).unwrap();
    let document = fixture.store.reload(fixture.host.as_ref()).await.unwrap();
    assert_eq!(&*document.text, external);
    assert_eq!(document.runtime_revision, 2);
    assert_eq!(fixture.host.revision(), 2);
    assert_eq!(fixture.disk_status(), DiskStatus::InSync);
    // reload 不写盘，也不写镜像。
    assert!(!fixture.mirror.exists());
}

#[tokio::test]
async fn persist_overwrites_external_edit_without_runtime_switch() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, "external: true\n").unwrap();
    let document = fixture.store.persist().await.unwrap();
    assert_eq!(fixture.file(), fixture.original);
    assert_eq!(
        fs::read_to_string(&fixture.mirror).unwrap(),
        fixture.original
    );
    assert_eq!(document.runtime_revision, 1);
    assert_eq!(fixture.host.revision(), 1);
    assert_eq!(fixture.disk_status(), DiskStatus::InSync);
}

#[tokio::test]
async fn initial_user_is_written_once_without_runtime_switch() {
    let fixture = Fixture::new();
    assert!(fixture.store.current().config.webui.users.is_empty());
    let users = fixture
        .store
        .create_initial_user("admin", PASSWORD_HASH)
        .await
        .unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].name, "admin");
    let document = fixture.store.current();
    assert_eq!(document.runtime_revision, 1);
    assert_eq!(document.config.webui.users.len(), 1);
    assert_eq!(fixture.file(), &*document.text);
    assert_eq!(fs::read_to_string(&fixture.mirror).unwrap(), fixture.file());
    assert_eq!(fixture.host.revision(), 1);
    assert!(matches!(
        fixture
            .store
            .create_initial_user("other", PASSWORD_HASH)
            .await,
        Err(StoreError::AlreadyInitialized)
    ));
}

#[tokio::test]
async fn initial_user_refuses_changed_disk_and_keeps_document_on_write_failure() {
    let fixture = Fixture::new();
    let before = fixture.store.current();
    fs::write(&fixture.source, format!("{}\n# x\n", fixture.original)).unwrap();
    assert!(matches!(
        fixture
            .store
            .create_initial_user("admin", PASSWORD_HASH)
            .await,
        Err(StoreError::FileChanged(DiskStatus::Modified))
    ));
    fs::write(&fixture.source, &fixture.original).unwrap();
    set_readonly(&fixture.source, true);
    assert!(matches!(
        fixture
            .store
            .create_initial_user("admin", PASSWORD_HASH)
            .await,
        Err(StoreError::Persist(_))
    ));
    assert!(Arc::ptr_eq(&before, &fixture.store.current()));
}

#[tokio::test]
async fn stale_watcher_generation_is_discarded() {
    let fixture = Fixture::new();
    let stale = fixture.store.disk_generation();
    // store 自己读盘会推进代次。
    fixture.store.reload(fixture.host.as_ref()).await.unwrap();
    assert_ne!(fixture.store.disk_generation(), stale);
    fixture.store.observe_disk(stale, DiskObservation::Missing);
    assert_eq!(fixture.disk_status(), DiskStatus::InSync);
    fixture
        .store
        .observe_disk(fixture.store.disk_generation(), DiskObservation::Missing);
    assert_eq!(fixture.disk_status(), DiskStatus::Missing);
}
