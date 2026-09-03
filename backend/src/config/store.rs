//! Management API 对源配置执行首次用户事务的持久化边界。

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::load::{ConfigLoader, LoadOptions};
use super::migrate::deterministic_hash;
use super::resolve::ResolvedWebUiUser;
use super::source_edit::{InitialWebUiUser, create_initial_webui_user};

const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
const STALE_LOCK_AGE: Duration = Duration::from_secs(300);

/// 首次用户事务完成后的认证快照。
pub(crate) struct InitialUserCommit {
    pub(crate) users: Vec<ResolvedWebUiUser>,
}

/// 对单个 CLI 源配置串行执行 CAS 更新。
pub(crate) struct ConfigStore {
    source_path: PathBuf,
    snapshot_path: Option<PathBuf>,
    expected_fingerprint: Mutex<String>,
    self_written_fingerprint: Mutex<Option<String>>,
    transaction: Mutex<()>,
}

impl ConfigStore {
    pub(crate) fn new(
        source_path: PathBuf,
        snapshot_path: PathBuf,
        expected_fingerprint: String,
    ) -> Self {
        let snapshot_path = (source_path != snapshot_path).then_some(snapshot_path);
        Self {
            source_path,
            snapshot_path,
            expected_fingerprint: Mutex::new(expected_fingerprint),
            self_written_fingerprint: Mutex::new(None),
            transaction: Mutex::new(()),
        }
    }

    /// 仅在源文件未变化且仍无用户时提交首个 Argon2id hash。
    pub(crate) fn create_initial_user(
        &self,
        name: &str,
        password_hash: &str,
    ) -> Result<InitialUserCommit, ConfigStoreError> {
        let _transaction = self
            .transaction
            .lock()
            .map_err(|_| ConfigStoreError::LockPoisoned)?;
        let _file_lock = ConfigFileLock::acquire(&lock_path(&self.source_path))?;
        let source = read_bounded(&self.source_path)?;
        let fingerprint = deterministic_hash(&source);
        let expected = self
            .expected_fingerprint
            .lock()
            .map_err(|_| ConfigStoreError::LockPoisoned)?
            .clone();
        if fingerprint != expected {
            return Err(ConfigStoreError::Conflict);
        }

        let text = std::str::from_utf8(&source).map_err(|_| ConfigStoreError::InvalidSource)?;
        let candidate = create_initial_webui_user(
            text,
            InitialWebUiUser {
                name,
                password_hash,
            },
        )
        .map_err(|error| match error {
            super::source_edit::SourceEditError::AlreadyInitialized => {
                ConfigStoreError::AlreadyInitialized
            }
            _ => ConfigStoreError::UnsupportedSource,
        })?;
        let candidate_bytes = candidate.as_bytes();
        let loaded = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_candidate_bytes(candidate_bytes, &self.source_path)
            .map_err(|_| ConfigStoreError::CandidateRejected)?;
        if loaded.resolved.webui.users.len() != 1 || loaded.resolved.webui.users[0].name != name {
            return Err(ConfigStoreError::CandidateRejected);
        }

        let new_fingerprint = deterministic_hash(candidate_bytes);
        commit_candidate(
            &self.source_path,
            self.snapshot_path.as_deref(),
            &source,
            candidate_bytes,
        )?;
        *self
            .expected_fingerprint
            .lock()
            .map_err(|_| ConfigStoreError::LockPoisoned)? = new_fingerprint.clone();
        *self
            .self_written_fingerprint
            .lock()
            .map_err(|_| ConfigStoreError::LockPoisoned)? = Some(new_fingerprint);

        Ok(InitialUserCommit {
            users: loaded.resolved.webui.users.clone(),
        })
    }

    /// 接受一次已通过完整 reload 校验的源文件指纹，并识别 Management 自己的提交。
    pub(crate) fn observe_reload(&self, fingerprint: &str) -> bool {
        *self
            .expected_fingerprint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fingerprint.to_owned();
        let mut self_written = self
            .self_written_fingerprint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let matches = self_written.as_deref() == Some(fingerprint);
        if matches || self_written.is_some() {
            *self_written = None;
        }
        matches
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfigStoreError {
    #[error("configuration was modified concurrently")]
    Conflict,
    #[error("webui setup has already completed")]
    AlreadyInitialized,
    #[error("configuration transaction is busy")]
    Busy,
    #[error("configuration source is invalid")]
    InvalidSource,
    #[error("configuration source uses an unsupported YAML representation")]
    UnsupportedSource,
    #[error("candidate configuration was rejected")]
    CandidateRejected,
    #[error("configuration transaction journal is invalid")]
    InvalidJournal,
    #[error("configuration transaction state cannot be recovered safely")]
    RecoveryConflict,
    #[error("configuration transaction I/O failed")]
    Io(#[source] std::io::Error),
    #[error("configuration transaction lock was poisoned")]
    LockPoisoned,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionJournal {
    old_fingerprint: String,
    new_fingerprint: String,
    source_path: PathBuf,
    source_stage: PathBuf,
    snapshot_path: Option<PathBuf>,
    snapshot_stage: Option<PathBuf>,
}

/// 在正常配置加载前完成或清理上一进程留下的已校验事务。
pub(crate) fn recover_pending_transaction(source_path: &Path) -> Result<(), ConfigStoreError> {
    let source_path = absolute_source_path(source_path)?;
    let journal_path = journal_path(&source_path);
    if !journal_path.exists() {
        return Ok(());
    }
    let journal_bytes = read_bounded(&journal_path)?;
    let journal: TransactionJournal =
        serde_json::from_slice(&journal_bytes).map_err(|_| ConfigStoreError::InvalidJournal)?;
    if journal.source_path != source_path
        || journal.snapshot_path.is_some() != journal.snapshot_stage.is_some()
    {
        return Err(ConfigStoreError::InvalidJournal);
    }
    recover_target(
        &journal.source_path,
        &journal.source_stage,
        &journal.old_fingerprint,
        &journal.new_fingerprint,
    )?;
    if let (Some(path), Some(stage)) = (&journal.snapshot_path, &journal.snapshot_stage) {
        recover_target(
            path,
            stage,
            &journal.old_fingerprint,
            &journal.new_fingerprint,
        )?;
    }
    remove_if_exists(&journal.source_stage)?;
    if let Some(stage) = &journal.snapshot_stage {
        remove_if_exists(stage)?;
    }
    remove_if_exists(&journal_path)?;
    remove_if_exists(&lock_path(&source_path))?;
    Ok(())
}

fn absolute_source_path(path: &Path) -> Result<PathBuf, ConfigStoreError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(ConfigStoreError::Io)?
            .join(path)
    };
    Ok(super::resolve::lexical_normalize(&absolute))
}

fn commit_candidate(
    source_path: &Path,
    snapshot_path: Option<&Path>,
    old_source: &[u8],
    candidate: &[u8],
) -> Result<(), ConfigStoreError> {
    let source_stage = write_stage(source_path, candidate)?;
    let snapshot_stage = match snapshot_path {
        Some(path) => Some(write_stage(path, candidate)?),
        None => None,
    };
    let journal = TransactionJournal {
        old_fingerprint: deterministic_hash(old_source),
        new_fingerprint: deterministic_hash(candidate),
        source_path: source_path.to_owned(),
        source_stage: source_stage.clone(),
        snapshot_path: snapshot_path.map(Path::to_owned),
        snapshot_stage: snapshot_stage.clone(),
    };
    let journal_path = journal_path(source_path);
    write_new_file_atomically(
        &journal_path,
        &serde_json::to_vec(&journal).map_err(|_| ConfigStoreError::InvalidJournal)?,
    )?;

    replace_file(&source_stage, source_path)?;
    if let (Some(stage), Some(path)) = (snapshot_stage.as_deref(), snapshot_path) {
        replace_file(stage, path)?;
    }
    remove_if_exists(&journal_path)?;
    Ok(())
}

fn recover_target(
    target: &Path,
    stage: &Path,
    old_fingerprint: &str,
    new_fingerprint: &str,
) -> Result<(), ConfigStoreError> {
    let current = deterministic_hash(&read_bounded(target)?);
    if current == new_fingerprint {
        return Ok(());
    }
    if current != old_fingerprint {
        return Err(ConfigStoreError::RecoveryConflict);
    }
    let staged = read_bounded(stage)?;
    if deterministic_hash(&staged) != new_fingerprint {
        return Err(ConfigStoreError::RecoveryConflict);
    }
    replace_file(stage, target)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, ConfigStoreError> {
    let mut file = File::open(path).map_err(ConfigStoreError::Io)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ConfigStoreError::Io)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigStoreError::InvalidSource);
    }
    Ok(bytes)
}

fn write_stage(target: &Path, bytes: &[u8]) -> Result<PathBuf, ConfigStoreError> {
    let stage = unique_sibling(target, "stage")?;
    write_restricted(&stage, bytes)?;
    if let Ok(metadata) = fs::metadata(target) {
        fs::set_permissions(&stage, metadata.permissions()).map_err(ConfigStoreError::Io)?;
    }
    Ok(stage)
}

fn write_new_file_atomically(path: &Path, bytes: &[u8]) -> Result<(), ConfigStoreError> {
    let stage = unique_sibling(path, "journal")?;
    write_restricted(&stage, bytes)?;
    fs::rename(&stage, path).map_err(ConfigStoreError::Io)
}

fn write_restricted(path: &Path, bytes: &[u8]) -> Result<(), ConfigStoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(ConfigStoreError::Io)?;
    file.write_all(bytes).map_err(ConfigStoreError::Io)?;
    file.sync_all().map_err(ConfigStoreError::Io)
}

fn unique_sibling(path: &Path, label: &str) -> Result<PathBuf, ConfigStoreError> {
    let parent = path.parent().ok_or(ConfigStoreError::InvalidSource)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(ConfigStoreError::InvalidSource)?;
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| {
        ConfigStoreError::Io(std::io::Error::other(format!(
            "random source failed: {error}"
        )))
    })?;
    let suffix = u64::from_le_bytes(random);
    Ok(parent.join(format!(".{name}.fluxdns-{label}-{suffix:016x}")))
}

fn journal_path(source_path: &Path) -> PathBuf {
    let name = source_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("config");
    source_path.with_file_name(format!(".{name}.fluxdns-journal"))
}

fn lock_path(source_path: &Path) -> PathBuf {
    let name = source_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("config");
    source_path.with_file_name(format!(".{name}.fluxdns-lock"))
}

struct ConfigFileLock {
    path: PathBuf,
}

impl ConfigFileLock {
    fn acquire(path: &Path) -> Result<Self, ConfigStoreError> {
        match create_lock(path) {
            Ok(()) => Ok(Self {
                path: path.to_owned(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age >= STALE_LOCK_AGE);
                if !stale {
                    return Err(ConfigStoreError::Busy);
                }
                fs::remove_file(path).map_err(ConfigStoreError::Io)?;
                create_lock(path).map_err(ConfigStoreError::Io)?;
                Ok(Self {
                    path: path.to_owned(),
                })
            }
            Err(error) => Err(ConfigStoreError::Io(error)),
        }
    }
}

impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_lock(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    write!(file, "{}", std::process::id())?;
    file.sync_all()
}

fn remove_if_exists(path: &Path) -> Result<(), ConfigStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConfigStoreError::Io(error)),
    }
}

#[cfg(not(windows))]
fn replace_file(stage: &Path, target: &Path) -> Result<(), ConfigStoreError> {
    fs::rename(stage, target).map_err(ConfigStoreError::Io)
}

#[cfg(windows)]
fn replace_file(stage: &Path, target: &Path) -> Result<(), ConfigStoreError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let stage = stage
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            stage.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(ConfigStoreError::Io(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{ConfigStore, TransactionJournal};
    use crate::config::{ConfigLoader, LoadOptions};

    #[test]
    fn initial_user_commit_updates_source_and_snapshot_once() {
        let (source, work_path) = crate::config::test_support::portable_example();
        let root = work_path.with_extension("management-store");
        std::fs::create_dir_all(&root).unwrap();
        let source_path = root.join("source.yaml");
        let original_work_path = work_path.to_string_lossy().replace('\\', "/");
        let test_work_path = root.to_string_lossy().replace('\\', "/");
        let source = source.replace(&original_work_path, &test_work_path);
        std::fs::write(&source_path, source).unwrap();
        let loaded = ConfigLoader::new(LoadOptions::default())
            .load_from_path(&source_path)
            .unwrap();
        let store = Arc::new(ConfigStore::new(
            loaded.source_path.unwrap(),
            loaded.resolved.work.snapshot_path.clone(),
            loaded.resolved.input_hash.clone(),
        ));
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$2M7ZV4yI1YVh7VdXk9G97A";

        let commit = store.create_initial_user("admin", hash).unwrap();
        assert_eq!(commit.users.len(), 1);
        assert_eq!(commit.users[0].name, "admin");
        assert_eq!(
            std::fs::read(&source_path).unwrap(),
            std::fs::read(&loaded.resolved.work.snapshot_path).unwrap()
        );
        assert!(matches!(
            store.create_initial_user("other", hash),
            Err(super::ConfigStoreError::AlreadyInitialized)
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_completes_snapshot_after_source_was_replaced() {
        let (_, work_path) = crate::config::test_support::portable_example();
        let root = work_path.with_extension("management-recovery");
        std::fs::create_dir_all(&root).unwrap();
        let source_path = root.join("source.yaml");
        let snapshot_path = root.join("snapshot.yaml");
        let old = b"version: 1\n";
        let new = b"version: 2\n";
        std::fs::write(&source_path, old).unwrap();
        std::fs::write(&snapshot_path, old).unwrap();

        let source_stage = super::write_stage(&source_path, new).unwrap();
        let snapshot_stage = super::write_stage(&snapshot_path, new).unwrap();
        let journal = TransactionJournal {
            old_fingerprint: super::deterministic_hash(old),
            new_fingerprint: super::deterministic_hash(new),
            source_path: source_path.clone(),
            source_stage: source_stage.clone(),
            snapshot_path: Some(snapshot_path.clone()),
            snapshot_stage: Some(snapshot_stage),
        };
        super::write_new_file_atomically(
            &super::journal_path(&source_path),
            &serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        super::replace_file(&source_stage, &source_path).unwrap();

        super::recover_pending_transaction(&source_path).unwrap();

        assert_eq!(std::fs::read(&source_path).unwrap(), new);
        assert_eq!(std::fs::read(&snapshot_path).unwrap(), new);
        assert!(!super::journal_path(&source_path).exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
