use super::*;
use crate::config::{
    edit::ConfigChange,
    model::{LogLevelDto, LogsDto},
    store::active::BeginApply,
};
use std::{fs, path::PathBuf};

const SOURCE: &str = include_str!("../../../../../tests/fixtures/config-v2.yaml");

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    derived: PathBuf,
    store: ConfigStore,
}

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/p1-external-source-tests")
            .join(super::super::random_token().unwrap());
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.yaml");
        let derived = root.join("config.yaml");
        fs::write(&source, SOURCE).unwrap();
        fs::write(&derived, SOURCE).unwrap();
        let store = ConfigStore::with_active_source(source.clone(), SOURCE, 1).unwrap();
        Self {
            root,
            source,
            derived,
            store,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.store.active.lock().unwrap().take();
        let root = self.root.canonicalize().unwrap();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/p1-external-source-tests")
            .canonicalize()
            .unwrap();
        assert_eq!(root.parent(), Some(base.as_path()));
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn preview_binds_real_source_bytes_and_never_uses_the_derived_file_as_input() {
    let fixture = Fixture::new();
    let initial = fixture.store.active_snapshot().unwrap();
    fs::write(
        &fixture.source,
        SOURCE.replace("level: info", "level: warn"),
    )
    .unwrap();
    let preview = fixture.store.external_source().unwrap();
    assert_eq!(preview.external.unwrap().logs.level, LogLevelDto::Warn);
    assert_eq!(preview.active.logs.level, LogLevelDto::Info);
    assert_eq!(preview.expected.active, initial.revision);
    fs::write(
        &fixture.derived,
        SOURCE.replace("level: info", "level: error"),
    )
    .unwrap();
    let second = fixture.store.external_source().unwrap();
    assert_ne!(second.expected.files, preview.expected.files);
    assert_eq!(second.external.unwrap().logs.level, LogLevelDto::Warn);
    let current = fixture.store.active_snapshot().unwrap();
    assert_eq!(current.source, initial.source);
    assert_eq!(current.runtime_revision, initial.runtime_revision);
    assert_eq!(current.persisted_revision, initial.persisted_revision);
    fs::write(&fixture.source, "password: must-not-return\ninvalid: [").unwrap();
    assert!(matches!(
        fixture.store.external_source().unwrap().external,
        Err(ExternalSourceError::Invalid)
    ));
}

#[test]
fn a_second_content_or_identity_change_in_either_file_rejects_the_captured_preview() {
    for (source, same_content) in [(true, true), (true, false), (false, true), (false, false)] {
        let fixture = Fixture::new();
        let captured = fixture.store.capture_external_source().unwrap();
        let old_files = captured.observation.revision();
        let path = if source {
            &fixture.source
        } else {
            &fixture.derived
        };
        if same_content {
            let replacement = fixture.root.join("replacement.yaml");
            fs::copy(path, &replacement).unwrap();
            fs::remove_file(path).unwrap();
            fs::rename(replacement, path).unwrap();
        } else {
            fs::write(path, "external content").unwrap();
        }
        assert!(matches!(
            fixture.store.finish_external_source(captured),
            Err(ActiveError::FileConflict)
        ));
        assert_ne!(
            fixture.store.active_snapshot().unwrap().expected().files,
            old_files
        );
    }
}

#[test]
fn publication_during_preview_rejects_old_active_values_instead_of_assigning_new_revisions() {
    let fixture = Fixture::new();
    let captured = fixture.store.capture_external_source().unwrap();
    let expected = fixture.store.observe_files().unwrap().expected();
    let changes = [ConfigChange::Logs(LogsDto {
        enable: false,
        level: LogLevelDto::Warn,
        path: "./logs/fluxdns.log".into(),
    })];
    let validation = fixture
        .store
        .validate_edit("actor", &expected, &changes, false)
        .unwrap();
    let BeginApply::Accepted(mut permit) = fixture
        .store
        .begin_apply(
            "actor",
            "apply",
            &expected,
            &changes,
            false,
            &validation.token,
            &validation.impacts,
        )
        .unwrap()
    else {
        panic!("new operation expected");
    };
    permit.begin_runtime_apply().unwrap();
    // 模拟 Runtime owner 的成功回报，仅验证预览与活动状态发布交错，不宣称 DNS 联合接线。
    permit.applied(2).unwrap();
    assert!(matches!(
        fixture.store.finish_external_source(captured),
        Err(ActiveError::ActiveConflict)
    ));
}

#[test]
fn preview_is_busy_during_transactions_and_does_not_create_files_or_operations() {
    let fixture = Fixture::new();
    let count = fs::read_dir(&fixture.root).unwrap().count();
    let before = fixture.store.active_snapshot().unwrap().expected();
    let transaction = fixture.store.transaction.lock().unwrap();
    assert!(matches!(
        fixture.store.external_source(),
        Err(ActiveError::Busy)
    ));
    drop(transaction);
    for _ in 0..3 {
        fixture.store.external_source().unwrap();
    }
    assert_eq!(fixture.store.active_snapshot().unwrap().expected(), before);
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), count);
    let guard = fixture.store.active.lock().unwrap();
    let state = guard.as_ref().unwrap();
    assert!(state.operations.is_empty());
    assert!(state.validations.is_empty());
}
