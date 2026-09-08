//! ConfigStore 内的 v2 活动源、验证票据、有界操作记录与文件事务；Runtime 由服务 owner 回报。
#![allow(dead_code)] // P3 模块写入仍会继续消费部分细粒度 helper。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use thiserror::Error;

use super::observation::{ManagedObservation, sha256_digest};
use super::persistence::{ManagedProtection, Persistence, PersistenceError};
use super::{
    ConfigFileLock, ConfigStore, ConfigStoreError, InitialUserCommit, commit_candidate, lock_path,
    read_bounded,
};
use crate::config::contract::ConfigV2;
use crate::config::edit::{ConfigChange, EditError, SourceCandidate, build_candidate};
use crate::config::migrate::deterministic_hash;
use crate::config::resolve::resolve_config_v2;
use crate::config::source_edit::{InitialWebUiUser, create_initial_webui_user};

const MAX_RECORDS: usize = 1024;
const VALIDATION_TTL: Duration = Duration::from_secs(60);
const OPERATION_TTL: Duration = Duration::from_secs(1800);

pub(crate) mod external;

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

/// 只保留可公开的失败类别，不把文件路径、配置正文或底层错误字符串写进操作缓存。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OperationFailure {
    ValidationFailed,
    ActiveRevisionConflict,
    FileRevisionConflict,
    ApplyFailed,
    PersistenceFailed,
    CompensationFailed,
}

/// 每次状态转移冻结当时的结果版本；后续操作或外改不能改写旧 operation 的事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OperationSnapshot {
    Preparing,
    Applying,
    Persisting {
        active_revision: String,
    },
    AppliedUnpersisted {
        active_revision: String,
        persisted_revision: Option<String>,
        error: OperationFailure,
    },
    AppliedSynced {
        active_revision: String,
        persisted_revision: String,
    },
    Rejected {
        error: OperationFailure,
    },
    CompensationFailed {
        active_revision: Option<String>,
        error: OperationFailure,
    },
    Unknown,
}

impl OperationSnapshot {
    pub(crate) fn phase(&self) -> OperationPhase {
        match self {
            Self::Preparing => OperationPhase::Preparing,
            Self::Applying => OperationPhase::Applying,
            Self::Persisting { .. } => OperationPhase::Persisting,
            Self::AppliedUnpersisted { .. } => OperationPhase::AppliedUnpersisted,
            Self::AppliedSynced { .. } => OperationPhase::AppliedSynced,
            Self::Rejected { .. } => OperationPhase::Rejected,
            Self::CompensationFailed { .. } => OperationPhase::CompensationFailed,
            Self::Unknown => OperationPhase::Unknown,
        }
    }

    fn synced(snapshot: &ActiveSnapshot) -> Self {
        Self::AppliedSynced {
            active_revision: snapshot.revision.clone(),
            persisted_revision: snapshot.revision.clone(),
        }
    }

    fn persistence_failed(snapshot: &ActiveSnapshot, error: &PersistenceError) -> Self {
        match error {
            PersistenceError::CleanupRequired(_) => Self::CompensationFailed {
                active_revision: Some(snapshot.revision.clone()),
                error: OperationFailure::CompensationFailed,
            },
            PersistenceError::RecoveryRequired => Self::Unknown,
            _ => Self::AppliedUnpersisted {
                active_revision: snapshot.revision.clone(),
                persisted_revision: snapshot.persisted_revision.clone(),
                error: persistence_failure(error),
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActiveSnapshot {
    pub(crate) source: Arc<str>,
    pub(crate) config: Arc<ConfigV2>,
    pub(crate) revision: String,
    pub(crate) runtime_revision: u64,
    pub(crate) persisted_revision: Option<String>,
    persisted_observation: ManagedObservation,
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
        self.observation != self.persisted_observation
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
    result: OperationSnapshot,
    expires: Instant,
}

/// 管理投影读取一个锁内快照；不对外暴露活动正文，也不为查询重新构造运行态。
pub(crate) struct ConfigurationStatus {
    pub(crate) active: ActiveSnapshot,
    pub(crate) known_files: ManagedObservation,
    pub(crate) operation: Option<OperationSnapshot>,
}

pub(crate) struct ValidatedCandidate {
    pub(crate) token: String,
    pub(crate) expected: ExpectedRevisions,
    pub(crate) impacts: BTreeSet<Impact>,
    pub(crate) expires_in: Duration,
}

/// 只有 begin_apply 可以生成；调用方仍须完成资源/owner/socket prepare 后才能切换运行态。
pub(crate) struct ApplyPermit {
    store: Arc<ConfigStore>,
    operation_id: String,
    expected: ExpectedRevisions,
    next_revision: String,
    pub(crate) candidate: SourceCandidate,
    completed: bool,
}

pub(crate) enum BeginApply {
    Accepted(ApplyPermit),
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
    /// 正式启动只以 loader 已消费的正文建立该状态，不从文件二次构造运行权威。
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
                persisted_observation: observation.clone(),
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

    /// setup 是 v2 活动源上的唯一 P2 写操作；提交后同步更新认证所需用户和活动文件事实。
    pub(super) fn create_initial_user_v2(
        &self,
        name: &str,
        password_hash: &str,
    ) -> Result<InitialUserCommit, ConfigStoreError> {
        let _transaction = self.transaction.try_lock().map_err(|error| match error {
            std::sync::TryLockError::Poisoned(_) => ConfigStoreError::LockPoisoned,
            std::sync::TryLockError::WouldBlock => ConfigStoreError::Busy,
        })?;
        let _file_lock = ConfigFileLock::acquire(&lock_path(&self.source_path))?;
        let source = read_bounded(&self.source_path)?;
        let text = std::str::from_utf8(&source).map_err(|_| ConfigStoreError::InvalidSource)?;
        let active = self
            .active
            .lock()
            .map_err(|_| ConfigStoreError::LockPoisoned)?
            .as_ref()
            .map(|state| state.snapshot.clone())
            .ok_or(ConfigStoreError::CandidateRejected)?;
        if active.operation_id.is_some() {
            return Err(ConfigStoreError::Busy);
        }
        if source.as_slice() != active.source.as_bytes() {
            return Err(ConfigStoreError::Conflict);
        }
        let candidate = create_initial_webui_user(
            text,
            InitialWebUiUser {
                name,
                password_hash,
            },
        )
        .map_err(|error| match error {
            crate::config::source_edit::SourceEditError::AlreadyInitialized => {
                ConfigStoreError::AlreadyInitialized
            }
            _ => ConfigStoreError::UnsupportedSource,
        })?;
        let candidate_bytes = candidate.as_bytes();
        let config =
            ConfigV2::parse(candidate_bytes).map_err(|_| ConfigStoreError::CandidateRejected)?;
        let resolved = resolve_config_v2(
            &config,
            deterministic_hash(candidate_bytes),
            &self.source_path,
        )
        .map_err(|_| ConfigStoreError::CandidateRejected)?
        .resolved;
        if resolved.webui.users.len() != 1 || resolved.webui.users[0].name != name {
            return Err(ConfigStoreError::CandidateRejected);
        }

        commit_candidate(
            &self.source_path,
            self.snapshot_path.as_deref(),
            &source,
            candidate_bytes,
        )?;
        let fingerprint = sha256_digest(candidate_bytes);
        let observation =
            ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        if !observation.matches_content(&fingerprint) {
            return Err(ConfigStoreError::Conflict);
        }
        let protection =
            ManagedProtection::capture(&self.source_path, self.snapshot_path.as_deref())
                .map_err(|_| ConfigStoreError::CandidateRejected)?;
        let revision = random_token().map_err(|_| ConfigStoreError::CandidateRejected)?;
        let mut state = self
            .active
            .lock()
            .map_err(|_| ConfigStoreError::LockPoisoned)?;
        let state = state.as_mut().ok_or(ConfigStoreError::CandidateRejected)?;
        state.snapshot.source = Arc::from(candidate);
        state.snapshot.config = Arc::new(config);
        state.snapshot.revision = revision.clone();
        state.snapshot.persisted_revision = Some(revision);
        state.snapshot.persisted_observation = observation.clone();
        state.snapshot.observation = observation;
        state.snapshot.operation_id = None;
        state.protection = protection;
        *self
            .expected_fingerprint
            .lock()
            .map_err(|_| ConfigStoreError::LockPoisoned)? = fingerprint.clone();
        *self
            .self_written_fingerprint
            .lock()
            .map_err(|_| ConfigStoreError::LockPoisoned)? = Some(fingerprint);
        Ok(InitialUserCommit {
            users: resolved.webui.users.clone(),
        })
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

    /// 接受后台 watcher 已完成的稳定观测；本入口不读取文件，也不改变活动配置或 Runtime。
    pub(crate) fn record_file_observation(
        &self,
        observation: ManagedObservation,
    ) -> Result<(), ActiveError> {
        let mut state = self.active.try_lock().map_err(|_| ActiveError::Busy)?;
        let state = state.as_mut().ok_or(ActiveError::Unavailable)?;
        state.snapshot.observation = observation;
        Ok(())
    }

    pub(crate) fn active_snapshot(&self) -> Result<ActiveSnapshot, ActiveError> {
        self.active
            .lock()
            .map_err(|_| ActiveError::Busy)?
            .as_ref()
            .map(|state| state.snapshot.clone())
            .ok_or(ActiveError::Unavailable)
    }

    /// 仅读取最近观测；不在 HTTP 状态查询中执行同步文件 I/O。
    /// 未接入异步事务 owner 前，文件事务持锁期间返回 Busy 而不是阻塞 executor。
    pub(crate) fn configuration_status(&self) -> Result<ConfigurationStatus, ActiveError> {
        let guard = self.active.try_lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_ref().ok_or(ActiveError::Unavailable)?;
        let mut known_files = state.snapshot.persisted_observation.clone();
        if let Some(persistence) = state.persistence.as_ref() {
            let candidate_files = persistence.candidate_observation();
            // 自写识别要求完整文件身份及摘要匹配，不能忽略“下一次文件事件”。
            if state.snapshot.observation.source == candidate_files.source {
                known_files.source = candidate_files.source;
            }
            if state.snapshot.observation.derived == candidate_files.derived {
                known_files.derived = candidate_files.derived;
            }
        }
        Ok(ConfigurationStatus {
            active: state.snapshot.clone(),
            known_files,
            operation: state
                .snapshot
                .operation_id
                .as_ref()
                .and_then(|id| state.operations.get(id))
                .map(|record| record.result.clone()),
        })
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
        self: &Arc<Self>,
        actor: &str,
        operation_id: &str,
        expected: &ExpectedRevisions,
        changes: &[ConfigChange],
        discard_external_changes: bool,
        validation_token: &str,
        confirmations: &BTreeSet<Impact>,
    ) -> Result<BeginApply, ActiveError> {
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
                Ok(BeginApply::Existing(record.result.phase()))
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
                result: OperationSnapshot::Preparing,
                expires: now + OPERATION_TTL,
            },
        );
        state.snapshot.operation_id = Some(operation_id.to_owned());
        Ok(BeginApply::Accepted(ApplyPermit {
            store: Arc::clone(self),
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
        self.operation_snapshot(actor, operation_id)
            .map(|result| result.phase())
    }

    /// 查询已冻结的操作事实；不同调用者、过期和未找到统一返回 Unknown，不暴露其他会话。
    pub(crate) fn operation_snapshot(
        &self,
        actor: &str,
        operation_id: &str,
    ) -> Result<OperationSnapshot, ActiveError> {
        validate_token(actor)?;
        validate_token(operation_id)?;
        let guard = self.active.try_lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_ref().ok_or(ActiveError::Unavailable)?;
        Ok(state
            .operations
            .get(operation_id)
            .filter(|record| {
                record.actor == sha256_digest(actor.as_bytes())
                    && (record.expires > Instant::now()
                        || state.snapshot.operation_id.as_deref() == Some(operation_id))
            })
            .map_or(OperationSnapshot::Unknown, |record| record.result.clone()))
    }

    /// 仅持久化已成功应用的活动源；失败保留新运行态和 gate，重试不会再调用 Runtime。
    /// 同步文件 I/O 必须由配置事务 owner 调度，不应直接放进 HTTP handler 或 DNS 请求。
    pub(crate) fn persist_applied(
        &self,
        actor: &str,
        operation_id: &str,
    ) -> Result<ActiveSnapshot, ActiveError> {
        self.persist_current(actor, operation_id, None)
    }

    /// 显式继续当前未同步操作，重新核对双 revision；确认仅丢弃当前观测到的外改。
    /// 原 operation、调用者及活动源保持绑定，不能借重试换入新的配置或再次激活 Runtime。
    pub(crate) fn retry_persistence(
        &self,
        actor: &str,
        operation_id: &str,
        expected: &ExpectedRevisions,
        discard_external_changes: bool,
    ) -> Result<ActiveSnapshot, ActiveError> {
        self.persist_current(
            actor,
            operation_id,
            Some((expected, discard_external_changes)),
        )
    }

    fn persist_current(
        &self,
        actor: &str,
        operation_id: &str,
        confirmation: Option<(&ExpectedRevisions, bool)>,
    ) -> Result<ActiveSnapshot, ActiveError> {
        validate_token(actor)?;
        validate_token(operation_id)?;
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
        if record.result.phase() == OperationPhase::AppliedSynced {
            return Ok(state.snapshot.clone());
        }
        if !(record.result.phase() == OperationPhase::AppliedUnpersisted
            || (confirmation.is_none() && record.result.phase() == OperationPhase::Persisting))
            || state.snapshot.operation_id.as_deref() != Some(operation_id)
        {
            return Err(ActiveError::Busy);
        }
        if let Some((expected, _)) = confirmation {
            state.snapshot.observation =
                ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
            if expected.active != state.snapshot.revision {
                return Err(ActiveError::ActiveConflict);
            }
            if expected.files != state.snapshot.observation.revision() {
                return Err(ActiveError::FileConflict);
            }
        }
        let persistence = state.persistence.as_mut().ok_or(ActiveError::Unavailable)?;
        if let Some((_, discard)) = confirmation
            && persistence.needs_reconfirmation()?
        {
            if !discard {
                return Err(ActiveError::ExternalConfirmation);
            }
            if let Err(error) = persistence.reconfirm(
                &state.snapshot.observation,
                state.snapshot.source.as_bytes(),
            ) {
                state.operations.get_mut(operation_id).unwrap().result =
                    OperationSnapshot::persistence_failed(&state.snapshot, &error);
                return Err(error.into());
            }
        }
        state.operations.get_mut(operation_id).unwrap().result = OperationSnapshot::Persisting {
            active_revision: state.snapshot.revision.clone(),
        };
        let result = persistence.commit();
        state.snapshot.observation =
            ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        if let Err(error) = result {
            state.operations.get_mut(operation_id).unwrap().result =
                OperationSnapshot::persistence_failed(&state.snapshot, &error);
            return Err(error.into());
        }
        // commit 已核对两个替换结果；其后的外改由 observation 暴露，不重放已完成提交。
        state.snapshot.persisted_revision = Some(state.snapshot.revision.clone());
        state.snapshot.persisted_observation =
            state.persistence.as_ref().unwrap().candidate_observation();
        state.snapshot.operation_id = None;
        let record = state.operations.get_mut(operation_id).unwrap();
        record.result = OperationSnapshot::synced(&state.snapshot);
        record.expires = Instant::now() + OPERATION_TTL;
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
                Ok(record.result.phase())
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
                result: OperationSnapshot::Preparing,
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
                let result = if unresolved {
                    OperationSnapshot::CompensationFailed {
                        active_revision: Some(state.snapshot.revision.clone()),
                        error: OperationFailure::CompensationFailed,
                    }
                } else {
                    state.snapshot.operation_id = None;
                    OperationSnapshot::Rejected {
                        error: persistence_failure(&error),
                    }
                };
                let record = state.operations.get_mut(operation_id).unwrap();
                record.result = result;
                if !unresolved {
                    record.expires = Instant::now() + OPERATION_TTL;
                }
                return Err(error.into());
            }
        };
        state.persistence = Some(persistence);
        state.operations.get_mut(operation_id).unwrap().result = OperationSnapshot::Persisting {
            active_revision: state.snapshot.revision.clone(),
        };
        // 活动源已是权威运行配置，确认还原可以决定文件提交，不需要再次应用 DNS。
        let result = state.persistence.as_mut().unwrap().commit();
        state.snapshot.observation =
            ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        if let Err(error) = result {
            state.operations.get_mut(operation_id).unwrap().result =
                OperationSnapshot::persistence_failed(&state.snapshot, &error);
            return Err(error.into());
        }
        state.snapshot.persisted_revision = Some(state.snapshot.revision.clone());
        state.snapshot.persisted_observation =
            state.persistence.as_ref().unwrap().candidate_observation();
        state.snapshot.operation_id = None;
        state.protection = state.persistence.as_ref().unwrap().protection();
        state.persistence = None;
        let record = state.operations.get_mut(operation_id).unwrap();
        record.result = OperationSnapshot::synced(&state.snapshot);
        record.expires = Instant::now() + OPERATION_TTL;
        Ok(OperationPhase::AppliedSynced)
    }
}

impl ApplyPermit {
    /// 将已冻结候选编译为 Runtime 配置；仍不代表资源、socket 或进程 owner 已准备完成。
    pub(crate) fn resolve_runtime_config(
        &self,
    ) -> Result<Arc<crate::config::resolve::ResolvedConfig>, ActiveError> {
        resolve_config_v2(
            &self.candidate.config,
            deterministic_hash(self.candidate.source.as_bytes()),
            &self.store.source_path,
        )
        .map(|validated| validated.resolved)
        .map_err(EditError::from)
        .map_err(ActiveError::from)
    }

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
        if record.result.phase() != OperationPhase::Preparing {
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
        record.result = OperationSnapshot::Applying;
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
        if record.result.phase() != OperationPhase::Applying
            || runtime_revision <= state.snapshot.runtime_revision
        {
            return Err(ActiveError::ActiveConflict);
        }
        record.result = OperationSnapshot::Persisting {
            active_revision: self.next_revision.clone(),
        };
        state.snapshot.source = Arc::from(self.candidate.source.as_str());
        state.snapshot.config = Arc::new(self.candidate.config.clone());
        state.snapshot.revision = self.next_revision.clone();
        state.snapshot.runtime_revision = runtime_revision;
        self.completed = true;
        Ok(())
    }

    /// prepare 拒绝或运行应用已完整补偿才可释放 gate；补偿不完整必须保持阻塞。
    pub(crate) fn rejected(self, compensated: bool) -> Result<(), ActiveError> {
        self.reject_with(OperationFailure::ApplyFailed, compensated)
    }

    /// 由事务 owner 提供已知的拒绝类别；是否释放 gate 仍以真实补偿及旁文件清理为准。
    pub(crate) fn reject_with(
        mut self,
        error: OperationFailure,
        compensated: bool,
    ) -> Result<(), ActiveError> {
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
        record.result = if compensated {
            OperationSnapshot::Rejected { error }
        } else {
            OperationSnapshot::CompensationFailed {
                active_revision: None,
                error: OperationFailure::CompensationFailed,
            }
        };
        if compensated {
            record.expires = Instant::now() + OPERATION_TTL;
            state.snapshot.operation_id = None;
            state.persistence = None;
        }
        self.completed = true;
        cleanup?;
        Ok(())
    }
}

impl Drop for ApplyPermit {
    fn drop(&mut self) {
        if !self.completed
            && let Ok(mut guard) = self.store.active.lock()
            && let Some(state) = guard.as_mut()
            && let Some(record) = state.operations.get_mut(&self.operation_id)
        {
            // 被取消或 panic 的命令不能被重放；保留 gate，等待控制 owner 核对真实状态。
            record.result = OperationSnapshot::Unknown;
        }
    }
}

fn persistence_failure(error: &PersistenceError) -> OperationFailure {
    match error {
        PersistenceError::Conflict => OperationFailure::FileRevisionConflict,
        PersistenceError::CleanupRequired(_) => OperationFailure::CompensationFailed,
        _ => OperationFailure::PersistenceFailed,
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
