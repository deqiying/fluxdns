use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use thiserror::Error;
use tokio::task::JoinSet;

use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::Clock;

/// Runtime supervisor 使用的稳定 task 标识。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskId(String);

impl TaskId {
    pub fn new(value: impl Into<String>) -> Result<Self, TaskIdError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err(TaskIdError);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(TaskIdError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("task id must be a short ASCII identifier")]
pub struct TaskIdError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultLevel {
    RequestLocal,
    Degraded,
    FatalCandidate,
    FatalEndpoint,
    Fatal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartPolicy {
    Never,
    Transient { max_restarts: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSpec {
    pub id: TaskId,
    pub component: &'static str,
    pub fault_level: FaultLevel,
    pub restart_policy: RestartPolicy,
}

impl TaskSpec {
    pub fn new(
        id: impl Into<String>,
        component: &'static str,
        fault_level: FaultLevel,
        restart_policy: RestartPolicy,
    ) -> Result<Self, TaskIdError> {
        Ok(Self {
            id: TaskId::new(id)?,
            component,
            fault_level,
            restart_policy,
        })
    }
}

#[derive(Debug, Error)]
pub enum TaskError {
    #[error("task returned a transient failure")]
    Transient,
    #[error("task returned a fatal failure")]
    Fatal,
    #[error("task observed supervisor cancellation")]
    Cancelled,
    #[error("task panicked")]
    Panicked,
}

pub type TaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskExit {
    Completed,
    Failed(TaskErrorKind),
    Cancelled,
    Panicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskErrorKind {
    Transient,
    Fatal,
    Panicked,
}

impl From<TaskError> for TaskExit {
    fn from(error: TaskError) -> Self {
        match error {
            TaskError::Transient => Self::Failed(TaskErrorKind::Transient),
            TaskError::Fatal => Self::Failed(TaskErrorKind::Fatal),
            TaskError::Cancelled => Self::Cancelled,
            TaskError::Panicked => Self::Panicked,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCompletion {
    pub spec: TaskSpec,
    pub exit: TaskExit,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub completed: u32,
    pub cancelled: u32,
    pub failed: u32,
    pub panicked: u32,
    pub aborted: u32,
    pub deadline_expired: bool,
}

impl ShutdownReport {
    pub fn task_count(self) -> u32 {
        self.completed + self.cancelled + self.failed + self.panicked + self.aborted
    }
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("task id is already registered: {0}")]
    DuplicateTask(TaskId),
}

/// 持有完整 task tree 的 supervisor；不会产生无人持有的 detached task。
pub struct Supervisor {
    cancellation: Cancellation,
    tasks: JoinSet<TaskCompletion>,
    registered: BTreeSet<TaskId>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            cancellation: Cancellation::new(),
            tasks: JoinSet::new(),
            registered: BTreeSet::new(),
        }
    }

    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    pub fn task_count(&self) -> usize {
        self.registered.len()
    }

    pub fn spawn(&mut self, spec: TaskSpec, future: TaskFuture) -> Result<(), SupervisorError> {
        if !self.registered.insert(spec.id.clone()) {
            return Err(SupervisorError::DuplicateTask(spec.id));
        }
        self.tasks.spawn(async move {
            let spec_for_completion = spec.clone();
            let exit = match future.await {
                Ok(()) => TaskExit::Completed,
                Err(error) => error.into(),
            };
            TaskCompletion {
                spec: spec_for_completion,
                exit,
            }
        });
        Ok(())
    }

    /// 等待一个 task 结束，并从注册表移除它。
    pub async fn join_next(&mut self) -> Option<TaskCompletion> {
        let result = self.tasks.join_next().await?;
        match result {
            Ok(completion) => {
                self.registered.remove(&completion.spec.id);
                Some(completion)
            }
            Err(error) => {
                // A JoinError without an attached task descriptor can only happen when
                // the task panics or is aborted. Keep the tree accounting conservative.
                let id = self
                    .registered
                    .iter()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| TaskId::new("unknown").expect("static task id"));
                self.registered.remove(&id);
                Some(TaskCompletion {
                    spec: TaskSpec {
                        id,
                        component: "runtime",
                        fault_level: FaultLevel::Fatal,
                        restart_policy: RestartPolicy::Never,
                    },
                    exit: if error.is_panic() {
                        TaskExit::Panicked
                    } else {
                        TaskExit::Cancelled
                    },
                })
            }
        }
    }

    /// 触发 shutdown 并在截止时间前回收全部 task；超时后 abort 剩余 task。
    pub async fn shutdown(&mut self, clock: &dyn Clock, deadline: Deadline) -> ShutdownReport {
        self.cancellation.cancel(CancelReason::Shutdown);
        let mut report = ShutdownReport::default();

        while !self.tasks.is_empty() {
            tokio::select! {
                completion = self.join_next() => {
                    if let Some(completion) = completion {
                        record_exit(&mut report, completion.exit);
                    }
                }
                _ = clock.sleep_until(deadline) => {
                    report.deadline_expired = true;
                    report.aborted = self.tasks.len() as u32;
                    self.tasks.abort_all();
                    while self.join_next().await.is_some() {}
                    break;
                }
            }
        }

        report
    }
}

impl fmt::Debug for Supervisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Supervisor")
            .field("task_count", &self.task_count())
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish()
    }
}

fn record_exit(report: &mut ShutdownReport, exit: TaskExit) {
    match exit {
        TaskExit::Completed => report.completed += 1,
        TaskExit::Cancelled => report.cancelled += 1,
        TaskExit::Failed(_) => report.failed += 1,
        TaskExit::Panicked => report.panicked += 1,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime};

    use crate::dns::{CancelReason, Cancellation, Deadline};
    use crate::ports::effects::Clock;
    use crate::ports::testing::FakeClock;

    use super::{
        FaultLevel, RestartPolicy, ShutdownReport, Supervisor, SupervisorError, TaskError,
        TaskErrorKind, TaskExit, TaskSpec,
    };

    fn spec(id: &str) -> TaskSpec {
        TaskSpec::new(id, "test", FaultLevel::Degraded, RestartPolicy::Never).unwrap()
    }

    #[tokio::test]
    async fn supervisor_owns_tasks_and_reports_outcomes() {
        let mut supervisor = Supervisor::new();
        supervisor
            .spawn(spec("completed"), Box::pin(async { Ok(()) }))
            .unwrap();
        supervisor
            .spawn(
                spec("failed"),
                Box::pin(async { Err(TaskError::Transient) }),
            )
            .unwrap();
        supervisor
            .spawn(spec("panic"), Box::pin(async { panic!("test panic") }))
            .unwrap();

        let mut exits = Vec::new();
        while let Some(completion) = supervisor.join_next().await {
            exits.push(completion.exit);
            if exits.len() == 3 {
                break;
            }
        }

        assert_eq!(supervisor.task_count(), 0);
        assert!(exits.contains(&TaskExit::Completed));
        assert!(exits.contains(&TaskExit::Failed(TaskErrorKind::Transient)));
        assert!(exits.contains(&TaskExit::Panicked));
    }

    #[tokio::test]
    async fn duplicate_task_ids_are_rejected_without_detaching_the_first_task() {
        let mut supervisor = Supervisor::new();
        supervisor
            .spawn(spec("same"), Box::pin(async { Ok(()) }))
            .unwrap();
        assert!(matches!(
            supervisor.spawn(spec("same"), Box::pin(async { Ok(()) })),
            Err(SupervisorError::DuplicateTask(_))
        ));
        assert_eq!(supervisor.task_count(), 1);
        supervisor.join_next().await.unwrap();
        assert_eq!(supervisor.task_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_propagates_reason_and_waits_for_cooperative_tasks() {
        let mut supervisor = Supervisor::new();
        let cancellation = supervisor.cancellation();
        supervisor
            .spawn(
                spec("cooperative"),
                Box::pin(cooperative_task(cancellation.clone())),
            )
            .unwrap();
        let clock = FakeClock::new(Instant::now(), SystemTime::UNIX_EPOCH);
        let report = supervisor
            .shutdown(
                &clock,
                Deadline::new(clock.monotonic_now() + Duration::from_secs(1)),
            )
            .await;

        assert_eq!(cancellation.reason(), Some(CancelReason::Shutdown));
        assert_eq!(
            report,
            ShutdownReport {
                cancelled: 1,
                ..ShutdownReport::default()
            }
        );
        assert_eq!(supervisor.task_count(), 0);
    }

    async fn cooperative_task(cancellation: Cancellation) -> Result<(), TaskError> {
        cancellation.cancelled().await;
        Err(TaskError::Cancelled)
    }
}
