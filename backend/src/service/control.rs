//! 服务 owner 的有界配置应用命令；不提供停止/重启或任意执行能力。
#![allow(dead_code)] // BC-03 消费者已接线，v2 配置事务生产者待新版 Runtime prepare 闭合。

use std::time::Instant;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::dns::{Deadline, RuntimeRevision};
use crate::runtime::PreparedRuntime;

use super::ServiceReloadError;

// 最多一份候选排队；当前正在应用的候选另占一个 owner 执行槽。
const QUEUE_CAPACITY: usize = 1;

#[derive(Clone)]
pub(crate) struct ServiceControl {
    sender: mpsc::Sender<ApplyCommand>,
}

pub(super) struct ApplyCommand {
    pub(super) expected: RuntimeRevision,
    pub(super) prepared: PreparedRuntime,
    pub(super) deadline: Deadline,
    pub(super) reply: oneshot::Sender<Result<RuntimeRevision, ControlError>>,
}

/// 入队与实际应用是两个边界；接收回执失败不能被解释为“没有执行”。
pub(crate) struct ApplyReceipt {
    result: oneshot::Receiver<Result<RuntimeRevision, ControlError>>,
    deadline: Deadline,
}

#[derive(Debug, Error)]
pub(crate) enum ControlError {
    #[error("service configuration queue is full")]
    Busy,
    #[error("service configuration owner is unavailable")]
    Unavailable,
    #[error("configuration command expired before application")]
    Expired,
    #[error("runtime revision conflict: expected {expected:?}, current {actual:?}")]
    RevisionConflict {
        expected: RuntimeRevision,
        actual: RuntimeRevision,
    },
    #[error("prepared runtime does not immediately follow the expected revision")]
    InvalidCandidateRevision,
    #[error("configuration application failed: {0}")]
    Apply(#[source] ServiceReloadError),
    #[error("configuration outcome is unknown; query the owning configuration operation")]
    OutcomeUnknown,
}

impl ServiceControl {
    /// 只接纳已准备的运行时，队列满时不等待、不重试；入队失败不会改变运行状态。
    pub(crate) fn try_apply(
        &self,
        expected: RuntimeRevision,
        prepared: PreparedRuntime,
        deadline: Deadline,
    ) -> Result<ApplyReceipt, ControlError> {
        if deadline.is_expired(Instant::now()) {
            return Err(ControlError::Expired);
        }
        if expected.0.checked_add(1) != Some(prepared.snapshot().revision().0) {
            return Err(ControlError::InvalidCandidateRevision);
        }
        let (reply, result) = oneshot::channel();
        self.sender
            .try_send(ApplyCommand {
                expected,
                prepared,
                deadline,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ControlError::Busy,
                mpsc::error::TrySendError::Closed(_) => ControlError::Unavailable,
            })?;
        Ok(ApplyReceipt { result, deadline })
    }
}

impl ApplyReceipt {
    /// 等待仅消费当前回执；超时/断线必须由配置事务查询 operation，不能自动重新入队。
    pub(crate) async fn outcome(self) -> Result<RuntimeRevision, ControlError> {
        tokio::time::timeout(self.deadline.remaining(Instant::now()), self.result)
            .await
            .map_err(|_| ControlError::OutcomeUnknown)?
            .map_err(|_| ControlError::OutcomeUnknown)?
    }
}

pub(super) fn channel() -> (ServiceControl, mpsc::Receiver<ApplyCommand>) {
    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
    (ServiceControl { sender }, receiver)
}

#[cfg(test)]
mod tests;
