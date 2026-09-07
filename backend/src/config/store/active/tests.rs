use super::*;
use crate::config::model::LogsDto;
use crate::config::store::observation::FileObservation;
use std::fs;

const FIXTURE: &str = include_str!("../../../../tests/fixtures/config-v2.yaml");

#[test]
fn unresolved_persistence_results_are_not_classified_as_retryable_file_failure() {
    let fixture = Fixture::new();
    let snapshot = fixture.store.active_snapshot().unwrap();
    assert_eq!(
        OperationSnapshot::persistence_failed(
            &snapshot,
            &PersistenceError::CleanupRequired(Box::new(PersistenceError::Conflict))
        ),
        OperationSnapshot::CompensationFailed {
            active_revision: Some(snapshot.revision.clone()),
            error: OperationFailure::CompensationFailed,
        }
    );
    assert_eq!(
        OperationSnapshot::persistence_failed(&snapshot, &PersistenceError::RecoveryRequired),
        OperationSnapshot::Unknown
    );
    assert_eq!(
        OperationSnapshot::persistence_failed(
            &snapshot,
            &PersistenceError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        ),
        OperationSnapshot::AppliedUnpersisted {
            active_revision: snapshot.revision.clone(),
            persisted_revision: snapshot.persisted_revision.clone(),
            error: OperationFailure::PersistenceFailed,
        }
    );
    assert_eq!(
        OperationSnapshot::persistence_failed(&snapshot, &PersistenceError::Conflict),
        OperationSnapshot::AppliedUnpersisted {
            active_revision: snapshot.revision,
            persisted_revision: snapshot.persisted_revision,
            error: OperationFailure::FileRevisionConflict,
        }
    );
}

#[cfg(windows)]
#[test]
fn confirmed_retry_rebuilds_only_current_operation_after_external_changes() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    for restoring in [false, true] {
        let fixture = Fixture::new();
        let occupied;
        if restoring {
            fs::write(&fixture.source, b"external source").unwrap();
            let expected = fixture.store.observe_files().unwrap().expected();
            occupied = fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&fixture.derived)
                .unwrap();
            assert!(
                fixture
                    .store
                    .restore_files("session-a", "sync", &expected, true)
                    .is_err()
            );
        } else {
            let mut permit = fixture.permit("sync");
            permit.begin_runtime_apply().unwrap();
            permit.applied(2).unwrap();
            occupied = fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&fixture.derived)
                .unwrap();
            assert!(fixture.store.persist_applied("session-a", "sync").is_err());
        }
        drop(occupied);
        let active = fixture.store.active_snapshot().unwrap();
        fs::write(&fixture.source, b"changed again").unwrap();
        let stale = fixture.store.observe_files().unwrap().expected();
        fs::remove_file(&fixture.derived).unwrap();
        assert!(matches!(
            fixture
                .store
                .retry_persistence("session-b", "sync", &stale, true),
            Err(ActiveError::Unavailable)
        ));
        assert!(matches!(
            fixture
                .store
                .retry_persistence("session-a", "sync", &stale, true),
            Err(ActiveError::FileConflict)
        ));
        let expected = fixture.store.observe_files().unwrap().expected();
        let mut wrong_active = expected.clone();
        wrong_active.active = "another-revision".into();
        assert!(matches!(
            fixture
                .store
                .retry_persistence("session-a", "sync", &wrong_active, true),
            Err(ActiveError::ActiveConflict)
        ));
        assert!(matches!(
            fixture
                .store
                .retry_persistence("session-a", "sync", &expected, false),
            Err(ActiveError::ExternalConfirmation)
        ));
        assert_eq!(fs::read(&fixture.source).unwrap(), b"changed again");
        assert!(!fixture.derived.exists());
        let synced = fixture
            .store
            .retry_persistence("session-a", "sync", &expected, true)
            .unwrap();
        assert_eq!(synced.revision, active.revision);
        assert_eq!(synced.runtime_revision, active.runtime_revision);
        assert_eq!(synced.source, active.source);
        assert!(!synced.externally_changed());
        assert_eq!(
            fs::read_to_string(&fixture.source).unwrap(),
            active.source.as_ref()
        );
        assert_eq!(
            fs::read_to_string(&fixture.derived).unwrap(),
            active.source.as_ref()
        );
        fs::write(&fixture.source, b"after completed").unwrap();
        fixture
            .store
            .retry_persistence("session-a", "sync", &expected, true)
            .unwrap();
        assert_eq!(fs::read(&fixture.source).unwrap(), b"after completed");
    }
}

#[test]
fn restore_recreates_missing_managed_files_from_active_source() {
    for (source_missing, derived_missing) in [(true, false), (false, true), (true, true)] {
        let fixture = Fixture::new();
        let initial = fixture.store.active_snapshot().unwrap();
        if source_missing {
            fs::remove_file(&fixture.source).unwrap();
        }
        if derived_missing {
            fs::remove_file(&fixture.derived).unwrap();
        }
        let expected = fixture.store.observe_files().unwrap().expected();
        assert_eq!(
            fixture
                .store
                .restore_files("session-a", "missing", &expected, true)
                .unwrap(),
            OperationPhase::AppliedSynced
        );
        let restored = fixture.store.active_snapshot().unwrap();
        assert_eq!(restored.revision, initial.revision);
        assert_eq!(restored.runtime_revision, initial.runtime_revision);
        assert!(!restored.externally_changed());
        assert_eq!(fs::read_to_string(&fixture.source).unwrap(), FIXTURE);
        assert_eq!(fs::read_to_string(&fixture.derived).unwrap(), FIXTURE);
    }
}

#[test]
fn restore_never_replaces_directory_or_hard_link_and_keeps_new_operation_available() {
    let fixture = Fixture::new();
    fs::remove_file(&fixture.source).unwrap();
    fs::create_dir(&fixture.source).unwrap();
    fs::write(fixture.source.join("personal-file"), b"keep").unwrap();
    let expected = fixture.store.observe_files().unwrap().expected();
    assert!(
        fixture
            .store
            .restore_files("session-a", "directory", &expected, true)
            .is_err()
    );
    assert_eq!(
        fs::read(fixture.source.join("personal-file")).unwrap(),
        b"keep"
    );
    assert_eq!(
        fixture.store.operation("session-a", "directory").unwrap(),
        OperationPhase::Rejected
    );
    // 只移除本用例明确创建的文件与空目录，不自动清理未知内容。
    fs::remove_file(fixture.source.join("personal-file")).unwrap();
    fs::remove_dir(&fixture.source).unwrap();
    fs::hard_link(&fixture.derived, &fixture.source).unwrap();
    let expected = fixture.store.observe_files().unwrap().expected();
    assert!(
        fixture
            .store
            .restore_files("session-a", "hard-link", &expected, true)
            .is_err()
    );
    assert_eq!(fs::read_to_string(&fixture.source).unwrap(), FIXTURE);
    fs::remove_file(&fixture.source).unwrap();
    let expected = fixture.store.observe_files().unwrap().expected();
    fixture
        .store
        .restore_files("session-a", "after-correction", &expected, true)
        .unwrap();
}

#[test]
fn restore_uses_active_bytes_without_changing_runtime_or_replaying_operation() {
    let fixture = Fixture::new();
    let initial = fixture.store.active_snapshot().unwrap();
    fs::write(&fixture.source, b"invalid: [yaml").unwrap();
    fs::write(&fixture.derived, b"external copy").unwrap();
    let expected = fixture.store.observe_files().unwrap().expected();
    assert!(matches!(
        fixture
            .store
            .restore_files("session-a", "restore", &expected, false),
        Err(ActiveError::ExternalConfirmation)
    ));
    assert_eq!(
        fixture
            .store
            .restore_files("session-a", "restore", &expected, true)
            .unwrap(),
        OperationPhase::AppliedSynced
    );
    let restored = fixture.store.active_snapshot().unwrap();
    assert_eq!(restored.revision, initial.revision);
    assert_eq!(restored.runtime_revision, initial.runtime_revision);
    assert_eq!(restored.source, initial.source);
    assert!(!restored.externally_changed());
    assert_eq!(fs::read_to_string(&fixture.source).unwrap(), FIXTURE);
    assert_eq!(fs::read_to_string(&fixture.derived).unwrap(), FIXTURE);

    fs::write(&fixture.source, b"another external change").unwrap();
    assert_eq!(
        fixture
            .store
            .restore_files("session-a", "restore", &expected, true)
            .unwrap(),
        OperationPhase::AppliedSynced
    );
    assert_eq!(
        fs::read(&fixture.source).unwrap(),
        b"another external change"
    );
    assert!(matches!(
        fixture
            .store
            .restore_files("session-b", "restore", &expected, true),
        Err(ActiveError::OperationIdReused)
    ));
    assert!(matches!(
        fixture
            .store
            .restore_files("session-a", "restore", &expected, false),
        Err(ActiveError::OperationIdReused)
    ));
}

#[test]
fn restore_rejects_stale_file_and_active_revisions_before_writing() {
    let fixture = Fixture::new();
    fs::write(&fixture.source, b"external one").unwrap();
    let expected = fixture.store.observe_files().unwrap().expected();
    fs::write(&fixture.derived, b"external two").unwrap();
    assert!(matches!(
        fixture
            .store
            .restore_files("session-a", "stale-file", &expected, true),
        Err(ActiveError::FileConflict)
    ));
    let mut expected = fixture.store.observe_files().unwrap().expected();
    expected.active = "other-active-revision".into();
    assert!(matches!(
        fixture
            .store
            .restore_files("session-a", "stale-active", &expected, true),
        Err(ActiveError::ActiveConflict)
    ));
    assert_eq!(fs::read(&fixture.source).unwrap(), b"external one");
    assert_eq!(fs::read(&fixture.derived).unwrap(), b"external two");
}

#[cfg(windows)]
#[test]
fn restore_failure_stays_blocked_and_only_explicit_persistence_retry_writes_again() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let fixture = Fixture::new();
    let initial = fixture.store.active_snapshot().unwrap();
    fs::write(&fixture.source, b"external source").unwrap();
    fs::write(&fixture.derived, b"external copy").unwrap();
    let expected = fixture.store.observe_files().unwrap().expected();
    let occupied = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&fixture.derived)
        .unwrap();
    assert!(matches!(
        fixture
            .store
            .restore_files("session-a", "failed-restore", &expected, true),
        Err(ActiveError::Persistence(_))
    ));
    assert_eq!(fs::read_to_string(&fixture.source).unwrap(), FIXTURE);
    assert_eq!(fs::read(&fixture.derived).unwrap(), b"external copy");
    assert_eq!(
        fixture
            .store
            .operation("session-a", "failed-restore")
            .unwrap(),
        OperationPhase::AppliedUnpersisted
    );
    drop(occupied);
    assert_eq!(
        fixture
            .store
            .restore_files("session-a", "failed-restore", &expected, true)
            .unwrap(),
        OperationPhase::AppliedUnpersisted
    );
    assert_eq!(fs::read(&fixture.derived).unwrap(), b"external copy");
    let current = fixture.store.observe_files().unwrap();
    assert!(matches!(
        fixture
            .store
            .restore_files("session-a", "stacked", &current.expected(), true),
        Err(ActiveError::Busy)
    ));
    let restored = fixture
        .store
        .persist_applied("session-a", "failed-restore")
        .unwrap();
    assert_eq!(restored.revision, initial.revision);
    assert_eq!(restored.runtime_revision, initial.runtime_revision);
    assert_eq!(fs::read_to_string(&fixture.derived).unwrap(), FIXTURE);
}

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    derived: PathBuf,
    store: ConfigStore,
}

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../_fluxdns/p1-config-tests")
            .join(random_token().unwrap());
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.yaml");
        let derived = root.join("config.yaml");
        fs::write(&source, FIXTURE).unwrap();
        fs::write(&derived, FIXTURE).unwrap();
        let store = ConfigStore::with_active_source(source.clone(), FIXTURE, 1).unwrap();
        Self {
            root,
            source,
            derived,
            store,
        }
    }

    fn edit(&self) -> Vec<ConfigChange> {
        vec![ConfigChange::Logs(LogsDto {
            enable: true,
            level: crate::config::model::LogLevelDto::Warn,
            path: "./logs/other.log".into(),
        })]
    }

    fn permit(&self, id: &str) -> ApplyPermit<'_> {
        let expected = self.store.observe_files().unwrap().expected();
        let changes = self.edit();
        let validation = self
            .store
            .validate_edit("session-a", &expected, &changes, false)
            .unwrap();
        let BeginApply::Accepted(permit) = self
            .store
            .begin_apply(
                "session-a",
                id,
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
        permit
    }
}

#[test]
fn applied_operation_persists_original_source_and_repeated_sync_is_idempotent() {
    let fixture = Fixture::new();
    let old = fixture.store.active_snapshot().unwrap();
    let mut permit = fixture.permit("persist");
    permit.begin_runtime_apply().unwrap();
    let candidate = permit.candidate.source.clone();
    assert_eq!(fs::read_to_string(&fixture.source).unwrap(), FIXTURE);
    // 这里只模拟 Runtime owner 的成功回报；真实 v2 服务生产者仍未接线。
    permit.applied(2).unwrap();
    let before = fixture.store.active_snapshot().unwrap();
    assert_ne!(before.revision, old.revision);
    assert_eq!(before.persisted_revision, old.persisted_revision);
    let synced = fixture
        .store
        .persist_applied("session-a", "persist")
        .unwrap();
    assert_eq!(synced.runtime_revision, 2);
    assert_eq!(synced.persisted_revision.as_ref(), Some(&synced.revision));
    assert_eq!(synced.operation_id, None);
    assert_eq!(fs::read_to_string(&fixture.source).unwrap(), candidate);
    assert_eq!(fs::read_to_string(&fixture.derived).unwrap(), candidate);
    assert_eq!(
        fixture.store.operation("session-a", "persist").unwrap(),
        OperationPhase::AppliedSynced
    );
    assert_eq!(
        fixture
            .store
            .persist_applied("session-a", "persist")
            .unwrap()
            .expected(),
        synced.expected()
    );
}

#[cfg(windows)]
#[test]
fn applied_write_failure_preserves_new_active_state_and_retry_never_reapplies() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let fixture = Fixture::new();
    let mut permit = fixture.permit("retry");
    permit.begin_runtime_apply().unwrap();
    permit.applied(2).unwrap();
    let active = fixture.store.active_snapshot().unwrap();
    let occupied = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&fixture.derived)
        .unwrap();
    assert!(fixture.store.persist_applied("session-a", "retry").is_err());
    let failed = fixture.store.active_snapshot().unwrap();
    assert_eq!(failed.revision, active.revision);
    assert_eq!(failed.runtime_revision, 2);
    assert_eq!(failed.persisted_revision, active.persisted_revision);
    assert_eq!(
        fixture.store.operation("session-a", "retry").unwrap(),
        OperationPhase::AppliedUnpersisted
    );
    assert!(matches!(
        fixture
            .store
            .validate_edit("session-a", &failed.expected(), &fixture.edit(), true),
        Err(ActiveError::Busy)
    ));
    assert!(fixture.store.persist_applied("session-b", "retry").is_err());
    drop(occupied);
    let retried = fixture.store.persist_applied("session-a", "retry").unwrap();
    assert_eq!(retried.revision, active.revision);
    assert_eq!(retried.runtime_revision, 2);
    assert_eq!(retried.persisted_revision.as_ref(), Some(&active.revision));
}

#[test]
fn rejected_prepared_operation_discards_only_its_candidate_and_releases_the_gate() {
    let fixture = Fixture::new();
    let old = fixture.store.active_snapshot().unwrap();
    let mut permit = fixture.permit("reject");
    permit.begin_runtime_apply().unwrap();
    permit.rejected(true).unwrap();
    assert_eq!(
        fixture.store.operation("session-a", "reject").unwrap(),
        OperationPhase::Rejected
    );
    assert_eq!(
        fixture.store.active_snapshot().unwrap().expected(),
        old.expected()
    );
    assert_eq!(fs::read_to_string(&fixture.source).unwrap(), FIXTURE);
    assert_eq!(fs::read_to_string(&fixture.derived).unwrap(), FIXTURE);
    fixture.permit("next").rejected(true).unwrap();
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // 仅清理本用例随机创建、已验证归属的目录，不触碰个人运行目录。
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p1-config-tests");
        assert_eq!(self.root.parent(), Some(base.as_path()));
        self.store.active.lock().unwrap().take();
        let resolved = fs::canonicalize(&self.root).unwrap();
        let base = fs::canonicalize(base).unwrap();
        assert_eq!(resolved.parent(), Some(base.as_path()));
        fs::remove_dir_all(resolved).unwrap();
    }
}

#[test]
fn observation_tracks_both_files_without_loading_external_configuration() {
    let fixture = Fixture::new();
    let initial = fixture.store.active_snapshot().unwrap();
    fs::write(&fixture.derived, b"invalid: [").unwrap();
    let changed = fixture.store.observe_files().unwrap();
    assert_ne!(initial.expected().files, changed.expected().files);
    assert_eq!(initial.revision, changed.revision);
    assert_eq!(initial.source, changed.source);
    assert_eq!(changed.runtime_revision, 1);
    assert!(changed.externally_changed());
    fs::remove_file(&fixture.source).unwrap();
    assert_eq!(
        fixture.store.observe_files().unwrap().observation.source,
        FileObservation::Missing
    );
    fs::write(
        &fixture.source,
        vec![b' '; crate::config::contract::MAX_CONFIG_BYTES + 1],
    )
    .unwrap();
    assert_eq!(
        fixture.store.observe_files().unwrap().observation.source,
        FileObservation::Oversized
    );
}

#[test]
fn same_content_replacement_changes_identity_and_hardlinks_are_rejected() {
    let fixture = Fixture::new();
    let initial = fixture.store.active_snapshot().unwrap();
    let replacement = fixture.root.join("replacement.yaml");
    fs::write(&replacement, FIXTURE).unwrap();
    fs::remove_file(&fixture.source).unwrap();
    fs::rename(replacement, &fixture.source).unwrap();
    let replaced = fixture.store.observe_files().unwrap();
    assert_ne!(initial.expected().files, replaced.expected().files);
    // 相同正文但身份变更仍不是本进程自写，必须重新确认该文件版本。
    assert!(replaced.externally_changed());
    fs::hard_link(&fixture.source, fixture.root.join("alias.yaml")).unwrap();
    assert_eq!(
        fixture.store.observe_files().unwrap().observation.source,
        FileObservation::Unreadable
    );
}

#[test]
fn validation_binds_actor_both_revisions_content_and_confirmation() {
    let fixture = Fixture::new();
    let expected = fixture.store.active_snapshot().unwrap().expected();
    let changes = fixture.edit();
    let validation = fixture
        .store
        .validate_edit("session-a", &expected, &changes, false)
        .unwrap();
    assert!(
        fixture
            .store
            .begin_apply(
                "session-b",
                "op",
                &expected,
                &changes,
                false,
                &validation.token,
                &validation.impacts
            )
            .is_err()
    );
    let mut other = changes.clone();
    if let ConfigChange::Logs(logs) = &mut other[0] {
        logs.enable = false;
    }
    assert!(
        fixture
            .store
            .begin_apply(
                "session-a",
                "op",
                &expected,
                &other,
                false,
                &validation.token,
                &validation.impacts
            )
            .is_err()
    );
    fs::write(&fixture.derived, format!("{FIXTURE}\n# external\n")).unwrap();
    assert!(matches!(
        fixture.store.begin_apply(
            "session-a",
            "op",
            &expected,
            &changes,
            false,
            &validation.token,
            &validation.impacts
        ),
        Err(ActiveError::FileConflict)
    ));
    let expected = fixture.store.observe_files().unwrap().expected();
    assert!(matches!(
        fixture
            .store
            .validate_edit("session-a", &expected, &changes, false),
        Err(ActiveError::ExternalConfirmation)
    ));
    let validation = fixture
        .store
        .validate_edit("session-a", &expected, &changes, true)
        .unwrap();
    assert!(validation.impacts.contains(&Impact::DiscardExternalChanges));
    assert!(matches!(
        fixture.store.begin_apply(
            "session-a",
            "op",
            &expected,
            &changes,
            true,
            &validation.token,
            &BTreeSet::new()
        ),
        Err(ActiveError::MissingConfirmation)
    ));
    let BeginApply::Accepted(permit) = fixture
        .store
        .begin_apply(
            "session-a",
            "op",
            &expected,
            &changes,
            true,
            &validation.token,
            &validation.impacts,
        )
        .unwrap()
    else {
        panic!("not accepted")
    };
    assert_eq!(
        permit.candidate.config.logs.path.to_str(),
        Some("./logs/other.log")
    );
    assert!(!permit.candidate.source.contains("# external"));
    permit.rejected(true).unwrap();
}

#[test]
fn accepted_operation_is_idempotent_and_dropped_permit_remains_unknown_and_blocked() {
    let fixture = Fixture::new();
    let expected = fixture.store.active_snapshot().unwrap().expected();
    let changes = fixture.edit();
    let validation = fixture
        .store
        .validate_edit("session", &expected, &changes, false)
        .unwrap();
    let BeginApply::Accepted(permit) = fixture
        .store
        .begin_apply(
            "session",
            "op",
            &expected,
            &changes,
            false,
            &validation.token,
            &validation.impacts,
        )
        .unwrap()
    else {
        panic!("not accepted")
    };
    assert!(matches!(
        fixture.store.begin_apply(
            "session",
            "op",
            &expected,
            &changes,
            false,
            &validation.token,
            &validation.impacts
        ),
        Ok(BeginApply::Existing(OperationPhase::Preparing))
    ));
    assert!(matches!(
        fixture.store.begin_apply(
            "other",
            "op",
            &expected,
            &changes,
            false,
            &validation.token,
            &validation.impacts
        ),
        Err(ActiveError::OperationIdReused)
    ));
    assert_eq!(
        fixture.store.operation("other", "op").unwrap(),
        OperationPhase::Unknown
    );
    drop(permit);
    assert_eq!(
        fixture.store.operation("session", "op").unwrap(),
        OperationPhase::Unknown
    );
    assert!(matches!(
        fixture
            .store
            .validate_edit("session", &expected, &changes, false),
        Err(ActiveError::Busy)
    ));
}

#[test]
fn applied_state_is_separate_from_disk_and_failed_compensation_blocks() {
    let fixture = Fixture::new();
    let before = fixture.store.active_snapshot().unwrap();
    let changes = fixture.edit();
    let validation = fixture
        .store
        .validate_edit("session", &before.expected(), &changes, false)
        .unwrap();
    let BeginApply::Accepted(mut permit) = fixture
        .store
        .begin_apply(
            "session",
            "op",
            &before.expected(),
            &changes,
            false,
            &validation.token,
            &validation.impacts,
        )
        .unwrap()
    else {
        panic!("not accepted")
    };
    permit.begin_runtime_apply().unwrap();
    // 仅状态机测试：模拟服务控制 owner 的成功回报，不宣称此处真的改变 DNS。
    permit.applied(2).unwrap();
    let after = fixture.store.active_snapshot().unwrap();
    assert_ne!(before.revision, after.revision);
    assert_eq!(after.persisted_revision, before.persisted_revision);
    assert_eq!(after.runtime_revision, 2);
    assert_eq!(fs::read_to_string(&fixture.source).unwrap(), FIXTURE);
    assert_eq!(
        fixture.store.operation("session", "op").unwrap(),
        OperationPhase::Persisting
    );
    assert!(matches!(
        fixture
            .store
            .validate_edit("session", &after.expected(), &changes, true),
        Err(ActiveError::Busy)
    ));

    let fixture = Fixture::new();
    let expected = fixture.store.active_snapshot().unwrap().expected();
    let validation = fixture
        .store
        .validate_edit("session", &expected, &changes, false)
        .unwrap();
    let BeginApply::Accepted(permit) = fixture
        .store
        .begin_apply(
            "session",
            "op",
            &expected,
            &changes,
            false,
            &validation.token,
            &validation.impacts,
        )
        .unwrap()
    else {
        panic!("not accepted")
    };
    permit.rejected(false).unwrap();
    assert_eq!(
        fixture.store.operation("session", "op").unwrap(),
        OperationPhase::CompensationFailed
    );
    assert!(
        fixture
            .store
            .active_snapshot()
            .unwrap()
            .operation_id
            .is_some()
    );
}

#[test]
fn expiration_capacity_and_setup_share_the_operation_boundary() {
    let fixture = Fixture::new();
    let expected = fixture.store.active_snapshot().unwrap().expected();
    let changes = fixture.edit();
    let validation = fixture
        .store
        .validate_edit("session", &expected, &changes, false)
        .unwrap();
    fixture
        .store
        .active
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .validations
        .get_mut(&validation.token)
        .unwrap()
        .expires = Instant::now();
    assert!(matches!(
        fixture.store.begin_apply(
            "session",
            "op",
            &expected,
            &changes,
            false,
            &validation.token,
            &validation.impacts
        ),
        Err(ActiveError::ValidationExpired)
    ));
    {
        let mut state = fixture.store.active.lock().unwrap();
        let state = state.as_mut().unwrap();
        state.validations.clear();
        for index in 0..MAX_RECORDS {
            state.validations.insert(
                index.to_string(),
                ValidationRecord {
                    digest: String::new(),
                    expires: Instant::now() + VALIDATION_TTL,
                    impacts: BTreeSet::new(),
                },
            );
        }
    }
    assert!(matches!(
        fixture
            .store
            .validate_edit("session", &expected, &changes, false),
        Err(ActiveError::Busy)
    ));
    let _gate = fixture.store.transaction.lock().unwrap();
    assert!(matches!(
        fixture.store.create_initial_user("admin", "not-a-hash"),
        Err(crate::config::store::ConfigStoreError::Busy)
    ));
}

#[test]
fn operation_retention_restarts_at_completion_and_pins_in_progress_results() {
    let fixture = Fixture::new();
    let mut permit = fixture.permit("retained");
    permit.begin_runtime_apply().unwrap();
    {
        let mut guard = fixture.store.active.lock().unwrap();
        guard
            .as_mut()
            .unwrap()
            .operations
            .get_mut("retained")
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
    }
    assert_eq!(
        fixture.store.operation("session-a", "retained").unwrap(),
        OperationPhase::Applying
    );
    permit.applied(2).unwrap();
    let completed_after = Instant::now();
    fixture
        .store
        .persist_applied("session-a", "retained")
        .unwrap();
    {
        let mut guard = fixture.store.active.lock().unwrap();
        let record = guard
            .as_mut()
            .unwrap()
            .operations
            .get_mut("retained")
            .unwrap();
        assert!(record.expires >= completed_after + OPERATION_TTL);
        assert!(matches!(
            record.result,
            OperationSnapshot::AppliedSynced { .. }
        ));
        record.expires = Instant::now() - Duration::from_secs(1);
    }
    assert_eq!(
        fixture
            .store
            .operation_snapshot("session-a", "retained")
            .unwrap(),
        OperationSnapshot::Unknown
    );
    let permit = fixture.permit("rejected-retained");
    {
        let mut guard = fixture.store.active.lock().unwrap();
        guard
            .as_mut()
            .unwrap()
            .operations
            .get_mut("rejected-retained")
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
    }
    let completed_after = Instant::now();
    permit
        .reject_with(OperationFailure::ValidationFailed, true)
        .unwrap();
    let guard = fixture.store.active.lock().unwrap();
    let record = guard
        .as_ref()
        .unwrap()
        .operations
        .get("rejected-retained")
        .unwrap();
    assert!(record.expires >= completed_after + OPERATION_TTL);
    assert_eq!(
        record.result,
        OperationSnapshot::Rejected {
            error: OperationFailure::ValidationFailed
        }
    );
}

#[test]
fn state_and_operation_queries_do_not_wait_for_the_file_transaction_lock() {
    let fixture = Fixture::new();
    let guard = fixture.store.active.lock().unwrap();
    assert!(matches!(
        fixture.store.configuration_status(),
        Err(ActiveError::Busy)
    ));
    assert!(matches!(
        fixture.store.operation_snapshot("session-a", "operation"),
        Err(ActiveError::Busy)
    ));
    drop(guard);
    assert!(fixture.store.configuration_status().is_ok());
    assert_eq!(
        fixture
            .store
            .operation_snapshot("session-a", "operation")
            .unwrap(),
        OperationSnapshot::Unknown
    );
}

#[test]
fn binding_digest_is_sha256() {
    assert_eq!(
        super::sha256_digest(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    );
}
