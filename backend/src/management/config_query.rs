//! v2 配置状态的白名单投影；只消费 ConfigStore，不从 Runtime 反推活动源。
#![allow(dead_code)] // BC-30 内部读口，待 v2 生产 owner/认证装配闭合后注册 handler。

use super::contract::{
    ConfigState, ErrorCode, FileCondition, FileObservation, OperationId, OperationResult,
    OperationStatus, Revision, SyncCondition,
};
use crate::config::store::{
    ConfigStore,
    active::{ActiveError, OperationFailure, OperationSnapshot},
    observation::FileObservation as ObservedFile,
};

/// 状态查询不做文件 I/O；外部差异与持久化结果独立展示，不把文件变化解释成自动应用。
pub(crate) fn configuration_state(store: &ConfigStore) -> Result<ConfigState, ErrorCode> {
    let status = store.configuration_status().map_err(error_code)?;
    let active = status.active;
    let synchronization = match &status.operation {
        Some(OperationSnapshot::Preparing | OperationSnapshot::Applying) => SyncCondition::Applying,
        Some(OperationSnapshot::Persisting { .. }) => SyncCondition::Persisting,
        Some(OperationSnapshot::AppliedUnpersisted { .. }) => SyncCondition::AppliedUnpersisted,
        Some(_) => SyncCondition::Blocked,
        None if active.operation_id.is_none()
            && active.persisted_revision.as_ref() == Some(&active.revision) =>
        {
            SyncCondition::Synced
        }
        None => SyncCondition::Blocked,
    };
    Ok(ConfigState {
        active_revision: revision(active.revision)?,
        // RuntimeRevision 的 u64 只编码为十进制 opaque revision，不输出不安全 JSON number。
        runtime_revision: revision(active.runtime_revision.to_string())?,
        persisted_revision: active.persisted_revision.map(revision).transpose()?,
        observed_file_revision: revision(active.observation.revision())?,
        files: FileObservation {
            source: file_condition(&active.observation.source, Some(&status.known_files.source)),
            derived: active
                .observation
                .derived
                .as_ref()
                .map(|file| file_condition(file, status.known_files.derived.as_ref())),
        },
        synchronization,
        operation_id: active
            .operation_id
            .map(|id| OperationId::try_from(id).map_err(|_| ErrorCode::ServiceUnavailable))
            .transpose()?,
    })
}

/// adapter 仍须先鉴权；此读口按原调用者返回冻结结果，Unknown 不能触发自动重放。
pub(crate) fn operation_result(
    store: &ConfigStore,
    actor: &str,
    operation_id: &str,
) -> Result<OperationResult, ErrorCode> {
    let id =
        OperationId::try_from(operation_id.to_owned()).map_err(|_| ErrorCode::InvalidArgument)?;
    let result = store
        .operation_snapshot(actor, operation_id)
        .map_err(error_code)?;
    let status = match result {
        OperationSnapshot::Preparing => OperationStatus::Preparing {},
        OperationSnapshot::Applying => OperationStatus::Applying {},
        OperationSnapshot::Persisting { active_revision } => OperationStatus::Persisting {
            active_revision: revision(active_revision)?,
        },
        OperationSnapshot::AppliedSynced {
            active_revision,
            persisted_revision,
        } => OperationStatus::AppliedSynced {
            active_revision: revision(active_revision)?,
            persisted_revision: revision(persisted_revision)?,
        },
        OperationSnapshot::AppliedUnpersisted {
            active_revision,
            persisted_revision,
            error,
        } => OperationStatus::AppliedUnpersisted {
            active_revision: revision(active_revision)?,
            persisted_revision: persisted_revision.map(revision).transpose()?,
            error: failure_code(error),
        },
        OperationSnapshot::Rejected { error } => OperationStatus::Rejected {
            error: failure_code(error),
        },
        OperationSnapshot::CompensationFailed {
            active_revision,
            error,
        } => OperationStatus::CompensationFailed {
            active_revision: active_revision.map(revision).transpose()?,
            error: failure_code(error),
        },
        OperationSnapshot::Unknown => OperationStatus::Unknown {},
    };
    Ok(OperationResult {
        operation_id: id,
        status,
    })
}

fn revision(value: String) -> Result<Revision, ErrorCode> {
    Revision::try_from(value).map_err(|_| ErrorCode::ServiceUnavailable)
}

fn file_condition(current: &ObservedFile, known: Option<&ObservedFile>) -> FileCondition {
    match current {
        ObservedFile::Missing => FileCondition::Missing,
        ObservedFile::Unreadable => FileCondition::Unreadable,
        ObservedFile::Oversized => FileCondition::Oversized,
        ObservedFile::Readable { .. } if Some(current) == known => FileCondition::Unchanged,
        ObservedFile::Readable { .. } => FileCondition::Changed,
    }
}

fn failure_code(error: OperationFailure) -> ErrorCode {
    match error {
        OperationFailure::ValidationFailed => ErrorCode::ValidationFailed,
        OperationFailure::ActiveRevisionConflict => ErrorCode::ActiveRevisionConflict,
        OperationFailure::FileRevisionConflict => ErrorCode::FileRevisionConflict,
        OperationFailure::ApplyFailed => ErrorCode::ApplyFailed,
        OperationFailure::PersistenceFailed => ErrorCode::PersistenceFailed,
        OperationFailure::CompensationFailed => ErrorCode::CompensationFailed,
    }
}

fn error_code(error: ActiveError) -> ErrorCode {
    match error {
        ActiveError::Busy => ErrorCode::OperationBusy,
        ActiveError::InvalidToken => ErrorCode::InvalidArgument,
        ActiveError::ActiveConflict => ErrorCode::ActiveRevisionConflict,
        ActiveError::FileConflict => ErrorCode::FileRevisionConflict,
        ActiveError::Unavailable | ActiveError::Entropy => ErrorCode::ServiceUnavailable,
        ActiveError::ExternalConfirmation => ErrorCode::ExternalChangesRequireConfirmation,
        ActiveError::ValidationExpired => ErrorCode::ValidationExpired,
        ActiveError::OperationIdReused => ErrorCode::OperationIdReused,
        ActiveError::MissingConfirmation | ActiveError::Candidate(_) => ErrorCode::ValidationFailed,
        ActiveError::Persistence(_) => ErrorCode::PersistenceFailed,
    }
}

#[cfg(test)]
mod tests;
