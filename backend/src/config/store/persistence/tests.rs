use std::fs;
use std::path::PathBuf;

use super::*;

const OLD: &[u8] = include_bytes!("../../../../tests/fixtures/config-v2.yaml");
const NEW: &[u8] = concat!(
    include_str!("../../../../tests/fixtures/config-v2.yaml"),
    "\n# new source\n"
)
.as_bytes();

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    derived: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../_fluxdns/p1-journal-tests")
            .join(suffix);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.yaml");
        let derived = root.join("config.yaml");
        fs::write(&source, OLD).unwrap();
        fs::write(&derived, OLD).unwrap();
        Self {
            root,
            source,
            derived,
        }
    }

    fn prepare(&self) -> Persistence {
        Persistence::prepare(
            &self.source,
            Some(&self.derived),
            &ManagedObservation::read(&self.source, Some(&self.derived)),
            NEW,
        )
        .unwrap()
    }

    fn assert_bytes(&self, source: &[u8], derived: &[u8]) {
        assert_eq!(fs::read(&self.source).unwrap(), source);
        assert_eq!(fs::read(&self.derived).unwrap(), derived);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p1-journal-tests");
        assert_eq!(self.root.parent(), Some(base.as_path()));
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn prepare_does_not_touch_targets_and_discard_never_rolls_forward() {
    let fixture = Fixture::new();
    let transaction = fixture.prepare();
    fixture.assert_bytes(OLD, OLD);
    assert!(matches!(
        transaction.commit_source(),
        Err(PersistenceError::InvalidJournal)
    ));
    transaction.discard().unwrap();
    fixture.assert_bytes(OLD, OLD);
    assert!(!journal_path(&fixture.source).exists());
}

#[test]
fn decided_commit_finishes_both_files_and_keeps_owned_identity() {
    let fixture = Fixture::new();
    let mut transaction = fixture.prepare();
    transaction.commit().unwrap();
    fixture.assert_bytes(NEW, NEW);
    assert_eq!(
        files::stamp(&fixture.source).unwrap(),
        transaction.journal.source.staged
    );
    assert!(!journal_path(&fixture.source).exists());
}

#[test]
fn journal_os_lock_prevents_live_recovery_and_parallel_prepare() {
    let fixture = Fixture::new();
    let transaction = fixture.prepare();
    assert!(matches!(
        recover(&fixture.source, Some(&fixture.derived)),
        Err(PersistenceError::Busy)
    ));
    assert!(matches!(
        Persistence::prepare(
            &fixture.source,
            Some(&fixture.derived),
            &ManagedObservation::read(&fixture.source, Some(&fixture.derived)),
            NEW,
        ),
        Err(PersistenceError::Busy)
    ));
    assert!(matches!(
        Persistence::prepare(
            &fixture.derived,
            None,
            &ManagedObservation::read(&fixture.derived, None),
            NEW,
        ),
        Err(PersistenceError::Busy)
    ));
    drop(transaction);
    assert_eq!(
        recover(&fixture.source, Some(&fixture.derived)).unwrap(),
        RecoveryOutcome::PreparedDiscarded
    );
}

#[test]
fn unknown_external_content_or_identity_prevents_all_replacements() {
    for replace_identity in [false, true] {
        let fixture = Fixture::new();
        let mut transaction = fixture.prepare();
        if replace_identity {
            fs::remove_file(&fixture.derived).unwrap();
            fs::write(&fixture.derived, OLD).unwrap();
        } else {
            fs::write(&fixture.derived, b"external").unwrap();
        }
        assert!(matches!(
            transaction.commit(),
            Err(PersistenceError::Conflict)
        ));
        assert_eq!(fs::read(&fixture.source).unwrap(), OLD);
        assert_eq!(transaction.journal.phase, Phase::Prepared);
        drop(transaction);
        assert_eq!(
            recover(&fixture.source, Some(&fixture.derived)).unwrap(),
            RecoveryOutcome::PreparedDiscarded
        );
        assert_eq!(fs::read(&fixture.source).unwrap(), OLD);
    }
}

#[test]
fn partial_commit_with_unknown_external_content_is_not_recovered_blindly() {
    let fixture = Fixture::new();
    let mut transaction = fixture.prepare();
    transaction.decide().unwrap();
    transaction.commit_source().unwrap();
    fs::write(&fixture.derived, b"external").unwrap();
    drop(transaction);
    assert!(matches!(
        recover(&fixture.source, Some(&fixture.derived)),
        Err(PersistenceError::Conflict)
    ));
    fixture.assert_bytes(NEW, b"external");
    assert!(journal_path(&fixture.source).exists());
}

#[test]
fn hardlinks_in_targets_or_stages_and_corrupt_journals_are_rejected() {
    let fixture = Fixture::new();
    let transaction = fixture.prepare();
    fs::hard_link(
        stage_path(&fixture.source, &transaction.journal.nonce),
        fixture.root.join("alias"),
    )
    .unwrap();
    drop(transaction);
    assert!(matches!(
        recover(&fixture.source, Some(&fixture.derived)),
        Err(PersistenceError::Conflict)
    ));
    fixture.assert_bytes(OLD, OLD);
    fs::write(journal_path(&fixture.source), b"{ invalid }").unwrap();
    assert!(matches!(
        recover(&fixture.source, Some(&fixture.derived)),
        Err(PersistenceError::InvalidJournal)
    ));
    fixture.assert_bytes(OLD, OLD);
}

#[test]
fn mismatched_derived_scope_cannot_redirect_recovery() {
    let fixture = Fixture::new();
    let mut transaction = fixture.prepare();
    transaction.decide().unwrap();
    drop(transaction);
    let other = fixture.root.join("other.yaml");
    fs::write(&other, OLD).unwrap();
    assert!(matches!(
        recover(&fixture.source, Some(&other)),
        Err(PersistenceError::Conflict)
    ));
    assert!(matches!(
        recover(&fixture.source, None),
        Err(PersistenceError::InvalidJournal)
    ));
    fixture.assert_bytes(OLD, OLD);
    assert_eq!(fs::read(other).unwrap(), OLD);
}

#[test]
fn recovery_rejects_oversized_journal_and_same_source_is_a_single_file_transaction() {
    let fixture = Fixture::new();
    fs::write(
        journal_path(&fixture.source),
        vec![b' '; MAX_JOURNAL_BYTES + 1],
    )
    .unwrap();
    assert!(matches!(
        recover(&fixture.source, Some(&fixture.derived)),
        Err(PersistenceError::Io(error)) if error.kind() == std::io::ErrorKind::FileTooLarge
    ));
    fixture.assert_bytes(OLD, OLD);
    // 另一个固定源就是 work/config.yaml，不制造第二个目标。
    let mut single = Persistence::prepare(
        &fixture.derived,
        None,
        &ManagedObservation::read(&fixture.derived, None),
        NEW,
    )
    .unwrap();
    single.commit().unwrap();
    fixture.assert_bytes(OLD, NEW);
}

#[cfg(windows)]
#[test]
fn actual_windows_replace_failure_keeps_decision_and_retry_only_writes_files() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let fixture = Fixture::new();
    let mut transaction = fixture.prepare();
    let occupied = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&fixture.derived)
        .unwrap();
    assert!(matches!(transaction.commit(), Err(PersistenceError::Io(_))));
    fixture.assert_bytes(NEW, OLD);
    assert_eq!(transaction.journal.phase, Phase::CommitDecided);
    drop(occupied);
    transaction.commit().unwrap();
    fixture.assert_bytes(NEW, NEW);
}

#[cfg(windows)]
#[test]
fn decision_write_failure_is_retryable_and_does_not_replace_formal_files() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    for replace_stage in [false, true] {
        let fixture = Fixture::new();
        let mut transaction = fixture.prepare();
        let occupied = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(journal_path(&fixture.source))
            .unwrap();
        assert!(matches!(transaction.commit(), Err(PersistenceError::Io(_))));
        fixture.assert_bytes(OLD, OLD);
        assert_eq!(transaction.journal.phase, Phase::Prepared);
        drop(occupied);
        if replace_stage {
            let path = decision_path(&fixture.source, &transaction.journal.nonce);
            let bytes = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            fs::write(path, bytes).unwrap();
            assert!(matches!(
                transaction.commit(),
                Err(PersistenceError::Conflict)
            ));
            fixture.assert_bytes(OLD, OLD);
        } else {
            transaction.commit().unwrap();
            fixture.assert_bytes(NEW, NEW);
        }
    }
}

#[test]
fn completed_file_replacements_can_retry_journal_cleanup_without_replacing_again() {
    let fixture = Fixture::new();
    let mut transaction = fixture.prepare();
    transaction.decide().unwrap();
    transaction.commit_source().unwrap();
    transaction.commit_derived().unwrap();
    let before = ManagedObservation::read(&fixture.source, Some(&fixture.derived));
    transaction.commit().unwrap();
    assert_eq!(
        before,
        ManagedObservation::read(&fixture.source, Some(&fixture.derived))
    );
    fixture.assert_bytes(NEW, NEW);
}

#[cfg(windows)]
#[test]
fn windows_stages_keep_access_entries_and_reject_stream_or_trailing_dot_aliases() {
    let fixture = Fixture::new();
    let transaction = fixture.prepare();
    for (path, target) in transaction.targets() {
        let stage = stage_path(path, &transaction.journal.nonce);
        assert_eq!(files::access_entries(path), files::access_entries(&stage));
        files::require(&stage, &target.staged).unwrap();
    }
    transaction.discard().unwrap();
    let stream = fixture.root.join("source.yaml:alternate");
    fs::write(&stream, b"alternate").unwrap();
    assert!(read_file(&stream).is_err());
    assert!(read_file(&fixture.root.join("source.yaml.")).is_err());
    fixture.assert_bytes(OLD, OLD);
}

#[cfg(windows)]
#[test]
fn windows_junction_parent_is_rejected_before_creating_candidate_files() {
    let fixture = Fixture::new();
    let junction = fixture.root.join("alias");
    let status = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:FLUXDNS_TEST_LINK -Target $env:FLUXDNS_TEST_TARGET | Out-Null"])
        .env("FLUXDNS_TEST_LINK", &junction)
        .env("FLUXDNS_TEST_TARGET", &fixture.root)
        .status().unwrap();
    assert!(status.success());
    let source = junction.join("source.yaml");
    let derived = junction.join("config.yaml");
    assert!(
        Persistence::prepare(
            &source,
            Some(&derived),
            &ManagedObservation::read(&source, Some(&derived)),
            NEW,
        )
        .is_err()
    );
    // RemoveDirectory 只移除此测试 junction，不递归进入其目标。
    fs::remove_dir(&junction).unwrap();
    fixture.assert_bytes(OLD, OLD);
    assert!(!journal_path(&fixture.source).exists());
}

#[test]
fn process_crash_matrix_recovers_only_persisted_decisions() {
    for point in ["prepared", "decided", "source", "derived"] {
        let fixture = Fixture::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::store::persistence::tests::crash_worker",
                "--nocapture",
            ])
            .env("FLUXDNS_P1_JOURNAL_ROOT", &fixture.root)
            .env("FLUXDNS_P1_JOURNAL_POINT", point)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(73), "{point}");
        let outcome = recover(&fixture.source, Some(&fixture.derived)).unwrap();
        if point == "prepared" {
            assert_eq!(outcome, RecoveryOutcome::PreparedDiscarded);
            fixture.assert_bytes(OLD, OLD);
        } else {
            assert_eq!(outcome, RecoveryOutcome::CommittedFiles);
            fixture.assert_bytes(NEW, NEW);
        }
        assert!(!journal_path(&fixture.source).exists());
    }
}

#[test]
fn crash_worker() {
    let Some(root) = std::env::var_os("FLUXDNS_P1_JOURNAL_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p1-journal-tests");
    assert_eq!(root.parent(), Some(base.as_path()));
    let source = root.join("source.yaml");
    let derived = root.join("config.yaml");
    let mut transaction = Persistence::prepare(
        &source,
        Some(&derived),
        &ManagedObservation::read(&source, Some(&derived)),
        NEW,
    )
    .unwrap();
    let point = std::env::var("FLUXDNS_P1_JOURNAL_POINT").unwrap();
    if point != "prepared" {
        transaction.decide().unwrap();
    }
    if point == "source" || point == "derived" {
        transaction.commit_source().unwrap();
    }
    if point == "derived" {
        transaction.commit_derived().unwrap();
    }
    // 模拟进程直接退出，不运行 Persistence/File 的 Drop；不能用普通 scope drop 冒充 crash。
    std::process::exit(73);
}
