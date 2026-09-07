//! ConfigStore 内的 v2 活动源、验证票据、有界操作记录与文件事务；Runtime 由服务 owner 回报。
#![allow(dead_code)] // BC-02 内部入口；BC-03/29 服务控制与 BC-26 启动接线后移除。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use thiserror::Error;

use super::ConfigStore;
use super::observation::{ManagedObservation, sha256_digest};
use super::persistence::{ManagedProtection, Persistence, PersistenceError};
use crate::config::contract::ConfigV2;
use crate::config::edit::{ConfigChange, EditError, SourceCandidate, build_candidate};

const MAX_RECORDS: usize = 1024;
const VALIDATION_TTL: Duration = Duration::from_secs(60);
const OPERATION_TTL: Duration = Duration::from_secs(1800);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ExpectedRevisions {
    pub(crate) active: String,
    pub(crate) files: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub(crate) enum Impact {
    RenameReferences,
    ListenerRebind,
    RetentionShortening,
    DiscardExternalChanges,
}

/// 操作状态仅描述内部事实，不以函数返回或 HTTP 200 替代实际 owner 成功。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OperationPhase {
    Preparing,
    Applying,
    Persisting,
    AppliedUnpersisted,
    AppliedSynced,
    Rejected,
    CompensationFailed,
    Unknown,
}

#[derive(Clone)]
pub(crate) struct ActiveSnapshot {
    pub(crate) source: Arc<str>,
    pub(crate) config: Arc<ConfigV2>,
    pub(crate) revision: String,
    pub(crate) runtime_revision: u64,
    pub(crate) persisted_revision: Option<String>,
    persisted_fingerprint: String,
    pub(crate) observation: ManagedObservation,
    pub(crate) operation_id: Option<String>,
}

impl ActiveSnapshot {
    pub(crate) fn expected(&self) -> ExpectedRevisions {
        ExpectedRevisions {
            active: self.revision.clone(),
            files: self.observation.revision(),
        }
    }

    pub(crate) fn externally_changed(&self) -> bool {
        !self
            .observation
            .matches_content(&self.persisted_fingerprint)
    }
}

pub(super) struct ActiveState {
    snapshot: ActiveSnapshot,
    validations: BTreeMap<String, ValidationRecord>,
    operations: BTreeMap<String, OperationRecord>,
    persistence: Option<Persistence>,
    protection: ManagedProtection,
}

struct ValidationRecord {
    digest: String,
    expires: Instant,
    impacts: BTreeSet<Impact>,
}

struct OperationRecord {
    digest: String,
    actor: String,
    phase: OperationPhase,
    expires: Instant,
}

pub(crate) struct ValidatedCandidate {
    pub(crate) token: String,
    pub(crate) expected: ExpectedRevisions,
    pub(crate) impacts: BTreeSet<Impact>,
    pub(crate) expires_in: Duration,
}

/// 只有 begin_apply 可以生成；调用方仍须完成资源/owner/socket prepare 后才能切换运行态。
pub(crate) struct ApplyPermit<'a> {
    store: &'a ConfigStore,
    operation_id: String,
    expected: ExpectedRevisions,
    next_revision: String,
    pub(crate) candidate: SourceCandidate,
    completed: bool,
}

pub(crate) enum BeginApply<'a> {
    Accepted(ApplyPermit<'a>),
    Existing(OperationPhase),
}

#[derive(Debug, Error)]
pub(crate) enum ActiveError {
    #[error("v2 active source has not been attached")]
    Unavailable,
    #[error("configuration operation is busy or requires reconciliation")]
    Busy,
    #[error("active configuration revision conflict")]
    ActiveConflict,
    #[error("managed file revision conflict")]
    FileConflict,
    #[error("external changes require explicit confirmation")]
    ExternalConfirmation,
    #[error("validation token expired or is not bound to this command")]
    ValidationExpired,
    #[error("operation id is already bound to another command")]
    OperationIdReused,
    #[error("invalid operation or actor identifier")]
    InvalidToken,
    #[error("required impact confirmation is missing")]
    MissingConfirmation,
    #[error("configuration source candidate rejected: {0}")]
    Candidate(#[from] EditError),
    #[error("secure token generation failed")]
    Entropy,
    #[error("configuration persistence failed: {0}")]
    Persistence(#[from] PersistenceError),
}

impl ConfigStore {
    /// 在 v2 启动 owner 成功后，以产生该运行态的原始 bytes 建立活动源，绝不以重读文件替代。
    /// 本阶段只提供内部入口；正式 loader/Storage 仍未切换。
    pub(crate) fn with_active_source(
        source_path: PathBuf,
        source: &str,
        runtime_revision: u64,
    ) -> Result<Self, ActiveError> {
        let source_path = crate::config::resolve::lexical_normalize(&source_path);
        super::persistence::ensure_no_journal(&source_path)?;
        let config = ConfigV2::parse(source.as_bytes()).map_err(EditError::from)?;
        let paths = config
            .resolve_paths(&source_path)
            .map_err(EditError::from)?;
        let fingerprint = sha256_digest(source.as_bytes());
        let store = Self::new(
            source_path,
            paths.work.join("config.yaml"),
            fingerprint.clone(),
        );
        let observation =
            ManagedObservation::read(&store.source_path, store.snapshot_path.as_deref());
        // 初始化不声称恢复或同步任意文件；不一致交由启动/恢复 owner 处理。
        if !observation.matches_content(&fingerprint) {
            return Err(ActiveError::FileConflict);
        }
        let protection =
            ManagedProtection::capture(&store.source_path, store.snapshot_path.as_deref())?;
        if ManagedObservation::read(&store.source_path, store.snapshot_path.as_deref())
            != observation
        {
            return Err(ActiveError::FileConflict);
        }
        let revision = random_token()?;
        *store.active.lock().map_err(|_| ActiveError::Busy)? = Some(ActiveState {
            snapshot: ActiveSnapshot {
                source: Arc::from(source),
                config: Arc::new(config),
                persisted_revision: Some(revision.clone()),
                persisted_fingerprint: fingerprint,
                revision,
                runtime_revision,
                observation,
                operation_id: None,
            },
            validations: BTreeMap::new(),
            operations: BTreeMap::new(),
            persistence: None,
            protection,
        });
        Ok(store)
    }

    /// 文件变化只更新观测，绝不修改活动源、Runtime revision 或认证。
    pub(crate) fn observe_files(&self) -> Result<ActiveSnapshot, ActiveError> {
        let _transaction = self.transaction.try_lock().map_err(|_| ActiveError::Busy)?;
        let observation =
            ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        let mut state = self.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = state.as_mut().ok_or(ActiveError::Unavailable)?;
        state.snapshot.observation = observation;
        Ok(state.snapshot.clone())
    }

    pub(crate) fn active_snapshot(&self) -> Result<ActiveSnapshot, ActiveError> {
        self.active
            .lock()
            .map_err(|_| ActiveError::Busy)?
            .as_ref()
            .map(|state| state.snapshot.clone())
            .ok_or(ActiveError::Unavailable)
    }

    /// 票据绑定调用者、候选摘要、双 revision 和确认影响；只保存摘要，避免积累完整配置副本。
    pub(crate) fn validate_edit(
        &self,
        actor: &str,
        expected: &ExpectedRevisions,
        changes: &[ConfigChange],
        discard_external_changes: bool,
    ) -> Result<ValidatedCandidate, ActiveError> {
        validate_token(actor)?;
        let snapshot = self.observe_files()?;
        check_expected(&snapshot, expected, discard_external_changes)?;
        let candidate = build_candidate(&snapshot.source, &self.source_path, changes)?;
        let impacts = impacts(&snapshot, &candidate)?;
        let digest = command_digest(actor, expected, changes, discard_external_changes)?;
        let token = random_token()?;
        let mut state = self.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = state.as_mut().ok_or(ActiveError::Unavailable)?;
        check_expected(&state.snapshot, expected, discard_external_changes)?;
        let now = Instant::now();
        state.validations.retain(|_, record| record.expires > now);
        if state.validations.len() >= MAX_RECORDS {
            return Err(ActiveError::Busy);
        }
        state.validations.insert(
            token.clone(),
            ValidationRecord {
                digest,
                expires: now + VALIDATION_TTL,
                impacts: impacts.clone(),
            },
        );
        Ok(ValidatedCandidate {
            token,
            expected: expected.clone(),
            impacts,
            expires_in: VALIDATION_TTL,
        })
    }

    /// 幂等检查先于过期版本检查；相同操作只返回既有结果，不再次执行新增或改名。
    pub(crate) fn begin_apply(
        &self,
        actor: &str,
        operation_id: &str,
        expected: &ExpectedRevisions,
        changes: &[ConfigChange],
        discard_external_changes: bool,
        validation_token: &str,
        confirmations: &BTreeSet<Impact>,
    ) -> Result<BeginApply<'_>, ActiveError> {
        validate_token(actor)?;
        validate_token(operation_id)?;
        let _transaction = self.transaction.try_lock().map_err(|_| ActiveError::Busy)?;
        let digest = command_digest(actor, expected, changes, discard_external_changes)?;
        let operation_digest = sha256_digest(
            &serde_json::to_vec(&(&digest, validation_token, confirmations))
                .map_err(|_| EditError::UnsupportedSource)?,
        );
        let observation =
            ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        let mut guard = self.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_mut().ok_or(ActiveError::Unavailable)?;
        let now = Instant::now();
        state.operations.retain(|id, record| {
            record.expires > now || state.snapshot.operation_id.as_ref() == Some(id)
        });
        if let Some(record) = state.operations.get(operation_id) {
            return if record.digest == operation_digest {
                Ok(BeginApply::Existing(record.phase.clone()))
            } else {
                Err(ActiveError::OperationIdReused)
            };
        }
        state.snapshot.observation = observation;
        check_expected(&state.snapshot, expected, discard_external_changes)?;
        let validation = state
            .validations
            .get(validation_token)
            .filter(|record| record.expires > now && record.digest == digest)
            .ok_or(ActiveError::ValidationExpired)?;
        if !validation.impacts.is_subset(confirmations) {
            return Err(ActiveError::MissingConfirmation);
        }
        if state.operations.len() >= MAX_RECORDS {
            return Err(ActiveError::Busy);
        }
        let source = state.snapshot.source.clone();
        // 完整候选验证不持有活动状态锁；transaction 排除 setup 和其他配置写入。
        drop(guard);
        let candidate = build_candidate(&source, &self.source_path, changes)?;
        let next_revision = random_token()?;
        let mut guard = self.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_mut().ok_or(ActiveError::Unavailable)?;
        check_expected(&state.snapshot, expected, discard_external_changes)?;
        state.operations.insert(
            operation_id.to_owned(),
            OperationRecord {
                digest: operation_digest,
                actor: sha256_digest(actor.as_bytes()),
                phase: OperationPhase::Preparing,
                expires: now + OPERATION_TTL,
            },
        );
        state.snapshot.operation_id = Some(operation_id.to_owned());
        Ok(BeginApply::Accepted(ApplyPermit {
            store: self,
            operation_id: operation_id.into(),
            expected: expected.clone(),
            next_revision,
            candidate,
            completed: false,
        }))
    }

    /// 未知/过期不能被解释成“从未执行”；进行中的记录不随 TTL 淘汰。
    pub(crate) fn operation(
        &self,
        actor: &str,
        operation_id: &str,
    ) -> Result<OperationPhase, ActiveError> {
        let guard = self.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_ref().ok_or(ActiveError::Unavailable)?;
        // 操作查询仍须由管理端鉴权；actor 校验避免未来 adapter 传空身份。
        validate_token(actor)?;
        Ok(state
            .operations
            .get(operation_id)
            .filter(|record| {
                record.actor == sha256_digest(actor.as_bytes())
                    && (record.expires > Instant::now()
                        || state.snapshot.operation_id.as_deref() == Some(operation_id))
            })
            .map_or(OperationPhase::Unknown, |record| record.phase.clone()))
    }

    /// 仅持久化已成功应用的活动源；失败保留新运行态和 gate，重试不会再调用 Runtime。
    /// 同步文件 I/O 必须由配置事务 owner 调度，不应直接放进 HTTP handler 或 DNS 请求。
    pub(crate) fn persist_applied(
        &self,
        actor: &str,
        operation_id: &str,
    ) -> Result<ActiveSnapshot, ActiveError> {
        validate_token(actor)?;
        let _transaction = self.transaction.try_lock().map_err(|_| ActiveError::Busy)?;
        let mut guard = self.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_mut().ok_or(ActiveError::Unavailable)?;
        let record = state
            .operations
            .get(operation_id)
            .ok_or(ActiveError::Unavailable)?;
        if record.actor != sha256_digest(actor.as_bytes()) {
            return Err(ActiveError::Unavailable);
        }
        if record.phase == OperationPhase::AppliedSynced {
            return Ok(state.snapshot.clone());
        }
        if record.phase != OperationPhase::AppliedUnpersisted
            || state.snapshot.operation_id.as_deref() != Some(operation_id)
        {
            return Err(ActiveError::Busy);
        }
        let persistence = state.persistence.as_mut().ok_or(ActiveError::Unavailable)?;
        let result = persistence.commit();
        state.snapshot.observation =
            ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        result?;
        let fingerprint = sha256_digest(state.snapshot.source.as_bytes());
        // commit 已核对两个替换结果；其后的外改由 observation 暴露，不重放已完成提交。
        state.snapshot.persisted_revision = Some(state.snapshot.revision.clone());
        state.snapshot.persisted_fingerprint = fingerprint;
        state.snapshot.operation_id = None;
        state.operations.get_mut(operation_id).unwrap().phase = OperationPhase::AppliedSynced;
        state.protection = state.persistence.as_ref().unwrap().protection();
        state.persistence = None;
        Ok(state.snapshot.clone())
    }

    /// 以活动原文还原固定受管文件；不构造候选 Runtime，不改变 active/runtime revision。
    /// 相同 operation 返回既有结果，失败后的再次写盘必须经过显式同步重试。
    pub(crate) fn restore_files(
        &self,
        actor: &str,
        operation_id: &str,
        expected: &ExpectedRevisions,
        discard_external_changes: bool,
    ) -> Result<OperationPhase, ActiveError> {
        validate_token(actor)?;
        validate_token(operation_id)?;
        let digest = sha256_digest(
            &serde_json::to_vec(&("restore", actor, expected, discard_external_changes))
                .map_err(|_| EditError::UnsupportedSource)?,
        );
        let _transaction = self.transaction.try_lock().map_err(|_| ActiveError::Busy)?;
        let mut guard = self.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_mut().ok_or(ActiveError::Unavailable)?;
        let now = Instant::now();
        state.operations.retain(|id, record| {
            record.expires > now || state.snapshot.operation_id.as_ref() == Some(id)
        });
        if let Some(record) = state.operations.get(operation_id) {
            return if record.digest == digest {
                Ok(record.phase.clone())
            } else {
                Err(ActiveError::OperationIdReused)
            };
        }
        state.snapshot.observation =
            ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        check_expected(&state.snapshot, expected, discard_external_changes)?;
        if state.operations.len() >= MAX_RECORDS {
            return Err(ActiveError::Busy);
        }
        state.operations.insert(
            operation_id.into(),
            OperationRecord {
                digest,
                actor: sha256_digest(actor.as_bytes()),
                phase: OperationPhase::Preparing,
                expires: now + OPERATION_TTL,
            },
        );
        state.snapshot.operation_id = Some(operation_id.into());
        let prepared = Persistence::prepare_with_protection(
            &self.source_path,
            self.snapshot_path.as_deref(),
            &state.snapshot.observation,
            state.snapshot.source.as_bytes(),
            Some(&state.protection),
        );
        let persistence = match prepared {
            Ok(persistence) => persistence,
            Err(error) => {
                let unresolved = matches!(
                    error,
                    PersistenceError::CleanupRequired(_) | PersistenceError::RecoveryRequired
                ) || super::persistence::ensure_no_journal(&self.source_path)
                    .is_err();
                let phase = if unresolved {
                    OperationPhase::CompensationFailed
                } else {
                    state.snapshot.operation_id = None;
                    OperationPhase::Rejected
                };
                state.operations.get_mut(operation_id).unwrap().phase = phase;
                return Err(error.into());
            }
        };
        state.persistence = Some(persistence);
        state.operations.get_mut(operation_id).unwrap().phase = OperationPhase::Persisting;
        // 活动源已是权威运行配置，确认还原可以决定文件提交，不需要再次应用 DNS。
        let result = state.persistence.as_mut().unwrap().commit();
        state.snapshot.observation =
            ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        if let Err(error) = result {
            state.operations.get_mut(operation_id).unwrap().phase =
                OperationPhase::AppliedUnpersisted;
            return Err(error.into());
        }
        state.snapshot.persisted_revision = Some(state.snapshot.revision.clone());
        state.snapshot.persisted_fingerprint = sha256_digest(state.snapshot.source.as_bytes());
        state.snapshot.operation_id = None;
        state.protection = state.persistence.as_ref().unwrap().protection();
        state.persistence = None;
        state.operations.get_mut(operation_id).unwrap().phase = OperationPhase::AppliedSynced;
        Ok(OperationPhase::AppliedSynced)
    }
}

impl ApplyPermit<'_> {
    /// 建立 PREPARED 并复核双版本，之后才允许提交服务命令；正式发布前仍须再次核对。
    pub(crate) fn begin_runtime_apply(&mut self) -> Result<(), ActiveError> {
        let _transaction = self
            .store
            .transaction
            .try_lock()
            .map_err(|_| ActiveError::Busy)?;
        let observation =
            ManagedObservation::read(&self.store.source_path, self.store.snapshot_path.as_deref());
        let mut guard = self.store.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_mut().ok_or(ActiveError::Unavailable)?;
        if state.snapshot.revision != self.expected.active {
            return Err(ActiveError::ActiveConflict);
        }
        if observation.revision() != self.expected.files {
            return Err(ActiveError::FileConflict);
        }
        let record = state
            .operations
            .get_mut(&self.operation_id)
            .ok_or(ActiveError::Unavailable)?;
        if record.phase != OperationPhase::Preparing {
            return Err(ActiveError::Busy);
        }
        let persistence = Persistence::prepare_with_protection(
            &self.store.source_path,
            self.store.snapshot_path.as_deref(),
            &state.snapshot.observation,
            self.candidate.source.as_bytes(),
            Some(&state.protection),
        )?;
        state.persistence = Some(persistence);
        if ManagedObservation::read(&self.store.source_path, self.store.snapshot_path.as_deref())
            != state.snapshot.observation
        {
            return Err(ActiveError::FileConflict);
        }
        record.phase = OperationPhase::Applying;
        Ok(())
    }

    /// 仅由服务控制循环在 owner 和请求接入成功后调用；持久化由 BC-29 单独完成。
    pub(crate) fn applied(mut self, runtime_revision: u64) -> Result<(), ActiveError> {
        let mut guard = self.store.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_mut().ok_or(ActiveError::Unavailable)?;
        let record = state
            .operations
            .get_mut(&self.operation_id)
            .ok_or(ActiveError::Unavailable)?;
        if record.phase != OperationPhase::Applying
            || runtime_revision <= state.snapshot.runtime_revision
        {
            return Err(ActiveError::ActiveConflict);
        }
        record.phase = OperationPhase::AppliedUnpersisted;
        state.snapshot.source = Arc::from(self.candidate.source.as_str());
        state.snapshot.config = Arc::new(self.candidate.config.clone());
        state.snapshot.revision = self.next_revision.clone();
        state.snapshot.runtime_revision = runtime_revision;
        self.completed = true;
        Ok(())
    }

    /// prepare 拒绝或运行应用已完整补偿才可释放 gate；补偿不完整必须保持阻塞。
    pub(crate) fn rejected(mut self, compensated: bool) -> Result<(), ActiveError> {
        let mut guard = self.store.active.lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_mut().ok_or(ActiveError::Unavailable)?;
        let cleanup = if compensated {
            match state.persistence.as_ref() {
                Some(persistence) => persistence.discard().map(Some),
                None => {
                    super::persistence::ensure_no_journal(&self.store.source_path).map(|()| None)
                }
            }
        } else {
            Ok(None)
        };
        let compensated = compensated && cleanup.is_ok();
        let record = state
            .operations
            .get_mut(&self.operation_id)
            .ok_or(ActiveError::Unavailable)?;
        record.phase = if compensated {
            OperationPhase::Rejected
        } else {
            OperationPhase::CompensationFailed
        };
        if compensated {
            state.snapshot.operation_id = None;
            state.persistence = None;
        }
        self.completed = true;
        cleanup?;
        Ok(())
    }
}

impl Drop for ApplyPermit<'_> {
    fn drop(&mut self) {
        if !self.completed
            && let Ok(mut guard) = self.store.active.lock()
            && let Some(state) = guard.as_mut()
            && let Some(record) = state.operations.get_mut(&self.operation_id)
        {
            // 被取消或 panic 的命令不能被重放；保留 gate，等待控制 owner 核对真实状态。
            record.phase = OperationPhase::Unknown;
        }
    }
}

fn check_expected(
    snapshot: &ActiveSnapshot,
    expected: &ExpectedRevisions,
    discard: bool,
) -> Result<(), ActiveError> {
    if snapshot.revision != expected.active {
        return Err(ActiveError::ActiveConflict);
    }
    if snapshot.observation.revision() != expected.files {
        return Err(ActiveError::FileConflict);
    }
    if snapshot.operation_id.is_some() {
        return Err(ActiveError::Busy);
    }
    if snapshot.externally_changed() && !discard {
        return Err(ActiveError::ExternalConfirmation);
    }
    Ok(())
}

fn impacts(
    snapshot: &ActiveSnapshot,
    candidate: &SourceCandidate,
) -> Result<BTreeSet<Impact>, ActiveError> {
    let mut impacts = BTreeSet::new();
    if candidate.renamed {
        impacts.insert(Impact::RenameReferences);
    }
    // 真正物理 endpoint 差异由 BC-03 prepare 裁定；这里使用保守确认，不承诺可热应用。
    if serde_json::to_value(&snapshot.config.listener).map_err(|_| EditError::UnsupportedSource)?
        != serde_json::to_value(&candidate.config.listener)
            .map_err(|_| EditError::UnsupportedSource)?
    {
        impacts.insert(Impact::ListenerRebind);
    }
    let old = &snapshot.config.statistics.retention;
    let new = &candidate.config.statistics.retention;
    if new.days < old.days
        || new.grace_days < old.grace_days
        || new.reference_size_bytes < old.reference_size_bytes
    {
        impacts.insert(Impact::RetentionShortening);
    }
    if snapshot.externally_changed() {
        impacts.insert(Impact::DiscardExternalChanges);
    }
    Ok(impacts)
}

fn command_digest(
    actor: &str,
    expected: &ExpectedRevisions,
    changes: &[ConfigChange],
    discard: bool,
) -> Result<String, ActiveError> {
    Ok(sha256_digest(
        &serde_json::to_vec(&(actor, expected, changes, discard))
            .map_err(|_| EditError::UnsupportedSource)?,
    ))
}

fn validate_token(value: &str) -> Result<(), ActiveError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._:".contains(&byte))
    {
        return Err(ActiveError::InvalidToken);
    }
    Ok(())
}

fn random_token() -> Result<String, ActiveError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ActiveError::Entropy)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests;
