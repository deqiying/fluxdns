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
    old: Option<Stamp>,
    staged: Stamp,
}

#[derive(Clone)]
struct TargetProtection {
    path: PathBuf,
    parent: String,
    permissions: files::Permissions,
}

/// 还原缺失文件时使用最后一次受管状态的权限，不从任意外部路径继承。
#[derive(Clone)]
pub(super) struct ManagedProtection {
    source: TargetProtection,
    derived: Option<TargetProtection>,
}

impl ManagedProtection {
    pub(super) fn capture(source: &Path, derived: Option<&Path>) -> Result<Self, PersistenceError> {
        Ok(Self {
            source: TargetProtection::capture(source)?,
            derived: derived.map(TargetProtection::capture).transpose()?,
        })
    }
}

impl TargetProtection {
    fn capture(path: &Path) -> Result<Self, PersistenceError> {
        let before = files::stamp(path)?;
        let protection = Self {
            path: path.to_owned(),
            parent: files::parent_identity(path)?,
            permissions: files::Permissions::capture(path)?,
        };
        files::require(path, &before)?;
        Ok(protection)
    }

    fn for_current(
        path: &Path,
        old: Option<&Stamp>,
        fallback: Option<&Self>,
    ) -> Result<Self, PersistenceError> {
        let parent = files::parent_identity(path)?;
        if let Some(fallback) = fallback
            && (fallback.path != path || fallback.parent != parent)
        {
            return Err(PersistenceError::Conflict);
        }
        if let Some(old) = old {
            let protection = Self::capture(path)?;
            files::require(path, old)?;
            return Ok(protection);
        }
        fallback.cloned().ok_or(PersistenceError::Conflict)
    }
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
    #[serde(default)]
    retired: Vec<RetiredStage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StageRole {
    Source,
    Derived,
    Decision,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetiredStage {
    nonce: String,
    role: StageRole,
    stamp: Stamp,
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
    protection: Option<ManagedProtection>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RecoveryOutcome {
    Absent,
    PreparedDiscarded,
    CommittedFiles,
}

impl Persistence {
    /// 只返回 journal 绑定的候选身份，不把提交后读到的任意新文件误认作本进程自写。
    pub(super) fn candidate_observation(&self) -> ManagedObservation {
        let observe = |target: &Target| super::observation::FileObservation::Readable {
            identity: target.staged.identity.clone(),
            fingerprint: target.staged.fingerprint.clone(),
        };
        ManagedObservation {
            source: observe(&self.journal.source),
            derived: self.journal.derived.as_ref().map(observe),
        }
    }

    pub(super) fn prepare(
        source: &Path,
        derived: Option<&Path>,
        expected: &ManagedObservation,
        candidate: &[u8],
    ) -> Result<Self, PersistenceError> {
        Self::prepare_with_protection(source, derived, expected, candidate, None)
    }

    /// 缺失目标必须有进程先前捕获的权限与父目录身份；不可读/超限目标仍拒绝覆盖。
    pub(super) fn prepare_with_protection(
        source: &Path,
        derived: Option<&Path>,
        expected: &ManagedObservation,
        candidate: &[u8],
        protection: Option<&ManagedProtection>,
    ) -> Result<Self, PersistenceError> {
        if candidate.len() > crate::config::contract::MAX_CONFIG_BYTES {
            return Err(PersistenceError::InvalidJournal);
        }
        validate_candidate(candidate, source, derived)?;
        let source_old = files::optional_stamp(source)?;
        let derived_old = derived.map(files::optional_stamp).transpose()?.flatten();
        let source_protection = TargetProtection::for_current(
            source,
            source_old.as_ref(),
            protection.map(|value| &value.source),
        )?;
        let derived_protection = derived
            .map(|path| {
                TargetProtection::for_current(
                    path,
                    derived_old.as_ref(),
                    protection.and_then(|value| value.derived.as_ref()),
                )
            })
            .transpose()?;
        let protection = ManagedProtection {
            source: source_protection,
            derived: derived_protection,
        };
        let (lock, lock_stamp) =
            files::acquire_lock_with_permissions(source, Some(&protection.source.permissions))?;
        let derived_lock = derived
            .zip(protection.derived.as_ref())
            .map(|(path, protection)| {
                files::acquire_lock_with_permissions(path, Some(&protection.permissions))
            })
            .transpose()?;
        ensure_no_journal(source)?;
        if &ManagedObservation::read(source, derived) != expected {
            return Err(PersistenceError::Conflict);
        }
        if source_old
            .as_ref()
            .zip(derived_old.as_ref())
            .is_some_and(|(source, other)| other.identity == source.identity)
        {
            return Err(PersistenceError::Conflict);
        }
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|_| PersistenceError::InvalidJournal)?;
        let nonce = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let source_target = stage(source, &nonce, source_old, &protection.source, candidate)?;
        let derived_target = match (derived, protection.derived.as_ref()) {
            (Some(path), Some(protection)) => {
                match stage(path, &nonce, derived_old, protection, candidate) {
                    Ok(target) => Some(target),
                    Err(error) => {
                        files::remove_known(&stage_path(source, &nonce), &source_target.staged)
                            .map_err(|error| PersistenceError::CleanupRequired(Box::new(error)))?;
                        return Err(error);
                    }
                }
            }
            _ => None,
        };
        let journal = Journal {
            version: 2,
            nonce,
            phase: Phase::Prepared,
            candidate_fingerprint: sha256_digest(candidate),
            source: source_target,
            derived: derived_target,
            retired: Vec::new(),
        };
        let write_journal = || {
            verify_parent(source, &journal.source)?;
            files::require_optional(source, journal.source.old.as_ref())?;
            if let (Some(path), Some(target)) = (derived, &journal.derived) {
                verify_parent(path, target)?;
                files::require_optional(path, target.old.as_ref())?;
            }
            files::write_new_with_permissions(
                &journal_path(source),
                &protection.source.permissions,
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
            protection: Some(protection),
        };
        Ok(transaction)
    }

    /// 运行 owner 明确应用成功或用户确认还原活动源后调用；PREPARED 不能自行决定提交。
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
        files::replace(&stage_path, &path, &staged, Some(&self.journal_stamp))?;
        self.journal = next;
        self.journal_stamp = staged;
        self.decision_stage = None;
        Ok(())
    }

    pub(super) fn commit(&mut self) -> Result<(), PersistenceError> {
        self.decide()?;
        self.cleanup_retired()?;
        // 先核对两个目标，再逐文件核对和替换；没有跨文件或跨进程 CAS 保证。
        self.verify_targets()?;
        self.commit_source()?;
        self.commit_derived()?;
        self.finish()
    }

    /// 只区分目标文件是否仍为已知状态；journal/父目录错误不能被“确认外改”放行。
    pub(super) fn needs_reconfirmation(&self) -> Result<bool, PersistenceError> {
        self.verify_journal()?;
        for (path, target) in self.targets() {
            verify_parent(path, target)?;
            let current = files::optional_stamp(path)?;
            if current != target.old
                && !(self.journal.phase == Phase::CommitDecided
                    && current.as_ref() == Some(&target.staged))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 显式确认新双文件版本后，仅重建同一活动源的文件决策，不重新应用 Runtime。
    /// 新决策与旧决策原子替换；旧旁文件由新 journal 的受限角色列表负责回收。
    pub(super) fn reconfirm(
        &mut self,
        expected: &ManagedObservation,
        candidate: &[u8],
    ) -> Result<(), PersistenceError> {
        self.verify_journal()?;
        if sha256_digest(candidate) != self.journal.candidate_fingerprint {
            return Err(PersistenceError::Conflict);
        }
        validate_candidate(candidate, &self.source, self.derived.as_deref())?;
        if &ManagedObservation::read(&self.source, self.derived.as_deref()) != expected {
            return Err(PersistenceError::Conflict);
        }
        self.cleanup_retired()?;
        let mut retired = Vec::new();
        for (role, path, stamp) in [
            (
                StageRole::Source,
                Some(stage_path(&self.source, &self.journal.nonce)),
                Some(&self.journal.source.staged),
            ),
            (
                StageRole::Derived,
                self.derived
                    .as_ref()
                    .map(|path| stage_path(path, &self.journal.nonce)),
                self.journal.derived.as_ref().map(|target| &target.staged),
            ),
            (
                StageRole::Decision,
                Some(decision_path(&self.source, &self.journal.nonce)),
                self.decision_stage.as_ref(),
            ),
        ] {
            if let (Some(path), Some(stamp)) = (path, stamp)
                && let Some(current) = files::optional_stamp(&path)?
            {
                if &current != stamp {
                    return Err(PersistenceError::Conflict);
                }
                retired.push(RetiredStage {
                    nonce: self.journal.nonce.clone(),
                    role,
                    stamp: current,
                });
            }
        }
        let fallback = self.protection();
        let source_old = files::optional_stamp(&self.source)?;
        let derived_old = self
            .derived
            .as_deref()
            .map(files::optional_stamp)
            .transpose()?
            .flatten();
        let protection = ManagedProtection {
            source: TargetProtection::for_current(
                &self.source,
                source_old.as_ref(),
                Some(&fallback.source),
            )?,
            derived: self
                .derived
                .as_deref()
                .map(|path| {
                    TargetProtection::for_current(
                        path,
                        derived_old.as_ref(),
                        fallback.derived.as_ref(),
                    )
                })
                .transpose()?,
        };
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|_| PersistenceError::InvalidJournal)?;
        let nonce = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if nonce == self.journal.nonce {
            return Err(PersistenceError::InvalidJournal);
        }
        let source = stage(
            &self.source,
            &nonce,
            source_old,
            &protection.source,
            candidate,
        )?;
        let derived = match self.derived.as_deref().zip(protection.derived.as_ref()) {
            Some((path, protection)) => {
                match stage(path, &nonce, derived_old, protection, candidate) {
                    Ok(target) => Some(target),
                    Err(error) => {
                        files::remove_known(&stage_path(&self.source, &nonce), &source.staged)
                            .map_err(|error| PersistenceError::CleanupRequired(Box::new(error)))?;
                        return Err(error);
                    }
                }
            }
            None => None,
        };
        let next = Journal {
            version: 2,
            nonce,
            phase: Phase::CommitDecided,
            candidate_fingerprint: self.journal.candidate_fingerprint.clone(),
            source,
            derived,
            retired,
        };
        let decision = decision_path(&self.source, &next.nonce);
        let mut decision_stamp = None;
        let mut replace = || -> Result<Stamp, PersistenceError> {
            self.verify_journal()?;
            if &ManagedObservation::read(&self.source, self.derived.as_deref()) != expected {
                return Err(PersistenceError::Conflict);
            }
            for (path, target) in std::iter::once((self.source.as_path(), &next.source))
                .chain(self.derived.as_deref().zip(next.derived.as_ref()))
            {
                verify_parent(path, target)?;
                files::require_optional(path, target.old.as_ref())?;
            }
            let staged = files::write_new_with_permissions(
                &decision,
                &protection.source.permissions,
                &serde_json::to_vec(&next).map_err(|_| PersistenceError::InvalidJournal)?,
            )?;
            decision_stamp = Some(staged.clone());
            files::replace(
                &decision,
                &journal_path(&self.source),
                &staged,
                Some(&self.journal_stamp),
            )?;
            Ok(staged)
        };
        let next_stamp = match replace() {
            Ok(stamp) => stamp,
            Err(error) => {
                // 若决策替换结果已无法确定，不删除可能被新 journal 引用的候选。
                if files::stamp(&journal_path(&self.source)).ok().as_ref()
                    != Some(&self.journal_stamp)
                {
                    return Err(PersistenceError::RecoveryRequired);
                }
                let cleanup = || -> Result<(), PersistenceError> {
                    files::remove_known(
                        &stage_path(&self.source, &next.nonce),
                        &next.source.staged,
                    )?;
                    if let Some((path, target)) = self.derived.as_deref().zip(next.derived.as_ref())
                    {
                        files::remove_known(&stage_path(path, &next.nonce), &target.staged)?;
                    }
                    if let Some(stamp) = &decision_stamp {
                        files::remove_known(&decision, stamp)?;
                    }
                    Ok(())
                };
                cleanup().map_err(|error| PersistenceError::CleanupRequired(Box::new(error)))?;
                return Err(error);
            }
        };
        self.journal = next;
        self.journal_stamp = next_stamp;
        self.decision_stage = None;
        self.protection = Some(protection);
        Ok(())
    }

    fn cleanup_retired(&self) -> Result<(), PersistenceError> {
        self.verify_journal()?;
        for retired in &self.journal.retired {
            let (target_path, target) = match retired.role {
                StageRole::Source | StageRole::Decision => (&self.source, &self.journal.source),
                StageRole::Derived => self
                    .derived
                    .as_ref()
                    .zip(self.journal.derived.as_ref())
                    .ok_or(PersistenceError::InvalidJournal)?,
            };
            verify_parent(target_path, target)?;
            let path = if retired.role == StageRole::Decision {
                decision_path(target_path, &retired.nonce)
            } else {
                stage_path(target_path, &retired.nonce)
            };
            if files::optional_stamp(&path)?.is_some() {
                files::remove_known(&path, &retired.stamp)?;
            }
        }
        Ok(())
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
        let current = files::optional_stamp(path)?;
        if current.as_ref() == Some(&target.staged) {
            return Ok(());
        }
        if current != target.old {
            return Err(PersistenceError::Conflict);
        }
        files::replace(
            &stage_path(path, &self.journal.nonce),
            path,
            &target.staged,
            target.old.as_ref(),
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

    pub(super) fn protection(&self) -> ManagedProtection {
        self.protection
            .clone()
            .expect("live transaction owns captured permissions")
    }

    fn verify_targets(&self) -> Result<(), PersistenceError> {
        for (path, target) in self.targets() {
            verify_parent(path, target)?;
            let current = files::optional_stamp(path)?;
            if current == target.old {
                files::require(&stage_path(path, &self.journal.nonce), &target.staged)?;
            } else if self.journal.phase != Phase::CommitDecided
                || current.as_ref() != Some(&target.staged)
            {
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
        || !valid_nonce(&journal.nonce)
        || journal.derived.is_some() != derived.is_some()
        || journal.source.staged.fingerprint != journal.candidate_fingerprint
        || journal
            .derived
            .as_ref()
            .is_some_and(|target| target.staged.fingerprint != journal.candidate_fingerprint)
        || journal.retired.len() > 3
        || (journal.phase == Phase::Prepared && !journal.retired.is_empty())
        || journal.retired.iter().any(|stage| {
            !valid_nonce(&stage.nonce)
                || stage.nonce == journal.nonce
                || (stage.role == StageRole::Derived && derived.is_none())
        })
        || journal
            .retired
            .iter()
            .map(|stage| (&stage.nonce, stage.role))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != journal.retired.len()
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
        protection: None,
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
            let candidate_path = if files::optional_stamp(source)?.as_ref()
                == Some(&transaction.journal.source.staged)
            {
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

pub(crate) fn has_pending_recovery(source: &Path) -> Result<bool, PersistenceError> {
    journal_path(source)
        .try_exists()
        .map_err(PersistenceError::Io)
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

fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == 64
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn ensure_no_journal(source: &Path) -> Result<(), PersistenceError> {
    if journal_path(source).try_exists()? {
        return Err(PersistenceError::RecoveryRequired);
    }
    Ok(())
}

fn stage(
    path: &Path,
    nonce: &str,
    old: Option<Stamp>,
    protection: &TargetProtection,
    bytes: &[u8],
) -> Result<Target, PersistenceError> {
    let parent = files::parent_identity(path)?;
    if parent != protection.parent {
        return Err(PersistenceError::Conflict);
    }
    let staged = files::write_new_with_permissions(
        &stage_path(path, nonce),
        &protection.permissions,
        bytes,
    )?;
    let recheck = || {
        if files::parent_identity(path)? != parent {
            return Err(PersistenceError::Conflict);
        }
        files::require_optional(path, old.as_ref())
    };
    if let Err(error) = recheck() {
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
