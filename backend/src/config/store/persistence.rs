//! 应用后文件事务；PREPARED 只能清理，COMMIT_DECIDED 才能补齐正式文件。
#![allow(dead_code)] // v2 启动恢复入口随 BC-26 接线，当前由活动源内部事务消费。

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::observation::{ManagedObservation, read_file, read_file_limited, sha256_digest};

mod files;

const MAX_JOURNAL_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub(crate) enum PersistenceError {
    #[error("configuration persistence owner is busy")]
    Busy,
    #[error("managed configuration file identity or content conflict")]
    Conflict,
    #[error("configuration persistence journal is invalid")]
    InvalidJournal,
    #[error("configuration persistence already requires recovery")]
    RecoveryRequired,
    #[error("configuration candidate cleanup needs manual reconciliation")]
    CleanupRequired(#[source] Box<PersistenceError>),
    #[error("configuration persistence I/O failed")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stamp {
    identity: String,
    fingerprint: String,
    permissions: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Phase {
    Prepared,
    CommitDecided,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Target {
    parent: String,
    old: Stamp,
    staged: Stamp,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    nonce: String,
    phase: Phase,
    candidate_fingerprint: String,
    source: Target,
    derived: Option<Target>,
}

/// 路径只来自 ConfigStore 的固定目标；journal 不接受任意路径或任意文件删除指令。
pub(super) struct Persistence {
    source: PathBuf,
    derived: Option<PathBuf>,
    journal: Journal,
    journal_stamp: Stamp,
    decision_stage: Option<Stamp>,
    _lock: File,
    lock_stamp: Stamp,
    derived_lock: Option<(File, Stamp)>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RecoveryOutcome {
    Absent,
    PreparedDiscarded,
    CommittedFiles,
}

impl Persistence {
    pub(super) fn prepare(
        source: &Path,
        derived: Option<&Path>,
        expected: &ManagedObservation,
        candidate: &[u8],
    ) -> Result<Self, PersistenceError> {
        if candidate.len() > crate::config::contract::MAX_CONFIG_BYTES {
            return Err(PersistenceError::InvalidJournal);
        }
        validate_candidate(candidate, source, derived)?;
        let (lock, lock_stamp) = files::acquire_lock(source)?;
        let derived_lock = derived.map(files::acquire_lock).transpose()?;
        ensure_no_journal(source)?;
        if &ManagedObservation::read(source, derived) != expected {
            return Err(PersistenceError::Conflict);
        }
        let source_old = files::stamp(source)?;
        let derived_old = derived.map(files::stamp).transpose()?;
        if derived_old
            .as_ref()
            .is_some_and(|other| other.identity == source_old.identity)
        {
            return Err(PersistenceError::Conflict);
        }
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|_| PersistenceError::InvalidJournal)?;
        let nonce = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let source_target = stage(source, &nonce, source_old, candidate)?;
        let derived_target = match (derived, derived_old) {
            (Some(path), Some(old)) => match stage(path, &nonce, old, candidate) {
                Ok(target) => Some(target),
                Err(error) => {
                    files::remove_known(&stage_path(source, &nonce), &source_target.staged)
                        .map_err(|error| PersistenceError::CleanupRequired(Box::new(error)))?;
                    return Err(error);
                }
            },
            _ => None,
        };
        let journal = Journal {
            version: 2,
            nonce,
            phase: Phase::Prepared,
            candidate_fingerprint: sha256_digest(candidate),
            source: source_target,
            derived: derived_target,
        };
        let write_journal = || {
            verify_parent(source, &journal.source)?;
            files::require(source, &journal.source.old)?;
            if let (Some(path), Some(target)) = (derived, &journal.derived) {
                verify_parent(path, target)?;
                files::require(path, &target.old)?;
            }
            files::write_new(
                &journal_path(source),
                source,
                &serde_json::to_vec(&journal).map_err(|_| PersistenceError::InvalidJournal)?,
            )
        };
        let journal_stamp = match write_journal() {
            Ok(stamp) => stamp,
            Err(error) => {
                files::remove_known(&stage_path(source, &journal.nonce), &journal.source.staged)
                    .map_err(|error| PersistenceError::CleanupRequired(Box::new(error)))?;
                if let (Some(path), Some(target)) = (derived, &journal.derived) {
                    files::remove_known(&stage_path(path, &journal.nonce), &target.staged)
                        .map_err(|error| PersistenceError::CleanupRequired(Box::new(error)))?;
                }
                return Err(error);
            }
        };
        let transaction = Self {
            source: source.to_owned(),
            derived: derived.map(Path::to_owned),
            journal,
            journal_stamp,
            decision_stage: None,
            _lock: lock,
            lock_stamp,
            derived_lock,
        };
        Ok(transaction)
    }

    /// 必须在运行 owner 明确应用成功后调用；PREPARED 文件准备不能自行决定提交。
    pub(super) fn decide(&mut self) -> Result<(), PersistenceError> {
        self.verify_journal()?;
        self.verify_targets()?;
        if self.journal.phase == Phase::CommitDecided {
            return Ok(());
        }
        let mut next = self.journal.clone();
        next.phase = Phase::CommitDecided;
        let path = journal_path(&self.source);
        let stage_path = decision_path(&self.source, &self.journal.nonce);
        let bytes = serde_json::to_vec(&next).map_err(|_| PersistenceError::InvalidJournal)?;
        let staged = match &self.decision_stage {
            Some(stamp) => {
                files::require(&stage_path, stamp)?;
                stamp.clone()
            }
            None => {
                let stamp = files::write_new(&stage_path, &path, &bytes)?;
                self.decision_stage = Some(stamp.clone());
                stamp
            }
        };
        files::replace(&stage_path, &path, &staged, &self.journal_stamp)?;
        self.journal = next;
        self.journal_stamp = staged;
        self.decision_stage = None;
        Ok(())
    }

    pub(super) fn commit(&mut self) -> Result<(), PersistenceError> {
        self.decide()?;
        // 先核对两个目标，再逐文件核对和替换；没有跨文件或跨进程 CAS 保证。
        self.verify_targets()?;
        self.commit_source()?;
        self.commit_derived()?;
        self.finish()
    }

    fn commit_source(&self) -> Result<(), PersistenceError> {
        self.commit_target(&self.source, &self.journal.source)
    }

    fn commit_derived(&self) -> Result<(), PersistenceError> {
        if let (Some(path), Some(target)) = (&self.derived, &self.journal.derived) {
            self.commit_target(path, target)?;
        }
        Ok(())
    }

    fn commit_target(&self, path: &Path, target: &Target) -> Result<(), PersistenceError> {
        if self.journal.phase != Phase::CommitDecided {
            return Err(PersistenceError::InvalidJournal);
        }
        self.verify_journal()?;
        verify_parent(path, target)?;
        let current = files::stamp(path)?;
        if current == target.staged {
            return Ok(());
        }
        if current != target.old {
            return Err(PersistenceError::Conflict);
        }
        files::replace(
            &stage_path(path, &self.journal.nonce),
            path,
            &target.staged,
            &target.old,
        )
    }

    fn finish(&self) -> Result<(), PersistenceError> {
        self.verify_journal()?;
        for (path, target) in self.targets() {
            verify_parent(path, target)?;
            files::require(path, &target.staged)?;
        }
        files::remove_known(&journal_path(&self.source), &self.journal_stamp)
    }

    /// 拒绝候选只清理身份和摘要仍匹配的旁文件，不改正式文件。
    pub(super) fn discard(&self) -> Result<(), PersistenceError> {
        if self.journal.phase != Phase::Prepared {
            return Err(PersistenceError::InvalidJournal);
        }
        self.verify_journal()?;
        if let Some(stamp) = &self.decision_stage {
            files::remove_known(&decision_path(&self.source, &self.journal.nonce), stamp)?;
        }
        for (path, target) in self.targets() {
            verify_parent(path, target)?;
            let stage = stage_path(path, &self.journal.nonce);
            if stage.try_exists()? {
                files::remove_known(&stage, &target.staged)?;
            }
        }
        // 没有持久决策的决策旁文件不具备提交效力；异常残留留给人工处理。
        files::remove_known(&journal_path(&self.source), &self.journal_stamp)
    }

    fn targets(&self) -> impl Iterator<Item = (&Path, &Target)> {
        std::iter::once((self.source.as_path(), &self.journal.source))
            .chain(self.derived.as_deref().zip(self.journal.derived.as_ref()))
    }

    fn verify_targets(&self) -> Result<(), PersistenceError> {
        for (path, target) in self.targets() {
            verify_parent(path, target)?;
            let current = files::stamp(path)?;
            if current == target.old {
                files::require(&stage_path(path, &self.journal.nonce), &target.staged)?;
            } else if self.journal.phase != Phase::CommitDecided || current != target.staged {
                return Err(PersistenceError::Conflict);
            }
        }
        Ok(())
    }

    fn verify_journal(&self) -> Result<(), PersistenceError> {
        files::verify_lock(
            &sibling(&self.source, "lock"),
            &self._lock,
            &self.lock_stamp,
        )?;
        if let (Some(path), Some((file, stamp))) = (&self.derived, &self.derived_lock) {
            files::verify_lock(&sibling(path, "lock"), file, stamp)?;
        }
        files::require(&journal_path(&self.source), &self.journal_stamp)
    }
}

/// 只允许在新版 loader/owner 启动之前调用；不在运行进程内借恢复自动加载配置。
pub(crate) fn recover(
    source: &Path,
    derived: Option<&Path>,
) -> Result<RecoveryOutcome, PersistenceError> {
    let path = journal_path(source);
    if !path.try_exists()? {
        return Ok(RecoveryOutcome::Absent);
    }
    let (lock, lock_stamp) = files::acquire_lock(source)?;
    let derived_lock = derived.map(files::acquire_lock).transpose()?;
    let (identity, bytes) = read_file_limited(&path, MAX_JOURNAL_BYTES)?;
    let journal: Journal =
        serde_json::from_slice(&bytes).map_err(|_| PersistenceError::InvalidJournal)?;
    if journal.version != 2
        || journal.nonce.len() != 64
        || !journal
            .nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || journal.derived.is_some() != derived.is_some()
        || journal.source.staged.fingerprint != journal.candidate_fingerprint
        || journal
            .derived
            .as_ref()
            .is_some_and(|target| target.staged.fingerprint != journal.candidate_fingerprint)
    {
        return Err(PersistenceError::InvalidJournal);
    }
    let mut transaction = Persistence {
        source: source.to_owned(),
        derived: derived.map(Path::to_owned),
        journal,
        journal_stamp: files::stamp(&path)?,
        decision_stage: None,
        _lock: lock,
        lock_stamp,
        derived_lock,
    };
    if transaction.journal_stamp.identity != identity
        || transaction.journal_stamp.fingerprint != sha256_digest(&bytes)
    {
        return Err(PersistenceError::Conflict);
    }
    match transaction.journal.phase {
        Phase::Prepared => {
            transaction.discard()?;
            Ok(RecoveryOutcome::PreparedDiscarded)
        }
        Phase::CommitDecided => {
            transaction.verify_targets()?;
            let candidate_path = if files::stamp(source)? == transaction.journal.source.staged {
                source.to_owned()
            } else {
                stage_path(source, &transaction.journal.nonce)
            };
            let (_, candidate) = read_file(&candidate_path)?;
            if sha256_digest(&candidate) != transaction.journal.candidate_fingerprint {
                return Err(PersistenceError::Conflict);
            }
            validate_candidate(&candidate, source, derived)?;
            transaction.commit()?;
            Ok(RecoveryOutcome::CommittedFiles)
        }
    }
}

fn validate_candidate(
    candidate: &[u8],
    source: &Path,
    derived: Option<&Path>,
) -> Result<(), PersistenceError> {
    let source = crate::config::resolve::lexical_normalize(source);
    let config = crate::config::contract::ConfigV2::parse(candidate)
        .map_err(|_| PersistenceError::InvalidJournal)?;
    let paths = config
        .resolve_paths(&source)
        .map_err(|_| PersistenceError::InvalidJournal)?;
    let snapshot = paths.work.join("config.yaml");
    let expected = (snapshot != source).then_some(snapshot);
    if derived.map(crate::config::resolve::lexical_normalize) != expected {
        return Err(PersistenceError::InvalidJournal);
    }
    Ok(())
}

pub(super) fn ensure_no_journal(source: &Path) -> Result<(), PersistenceError> {
    if journal_path(source).try_exists()? {
        return Err(PersistenceError::RecoveryRequired);
    }
    Ok(())
}

fn stage(path: &Path, nonce: &str, old: Stamp, bytes: &[u8]) -> Result<Target, PersistenceError> {
    let parent = files::parent_identity(path)?;
    let staged = files::write_new(&stage_path(path, nonce), path, bytes)?;
    if let Err(error) = files::require(path, &old) {
        files::remove_known(&stage_path(path, nonce), &staged)
            .map_err(|error| PersistenceError::CleanupRequired(Box::new(error)))?;
        return Err(error);
    }
    Ok(Target {
        parent,
        old,
        staged,
    })
}

fn verify_parent(path: &Path, target: &Target) -> Result<(), PersistenceError> {
    if files::parent_identity(path)? != target.parent {
        return Err(PersistenceError::Conflict);
    }
    Ok(())
}

fn sibling(path: &Path, label: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.fluxdns-v2-{label}"))
}

fn journal_path(source: &Path) -> PathBuf {
    sibling(source, "journal")
}

fn stage_path(target: &Path, nonce: &str) -> PathBuf {
    sibling(target, &format!("{nonce}-stage"))
}

fn decision_path(source: &Path, nonce: &str) -> PathBuf {
    sibling(source, &format!("{nonce}-decision"))
}

#[cfg(test)]
mod tests;
