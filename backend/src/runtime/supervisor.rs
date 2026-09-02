use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::task::{Id as TokioTaskId, JoinSet};

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
    pub restart_count: u32,
}

impl TaskCompletion {
    pub fn restart_exhausted(&self) -> bool {
        matches!(self.exit, TaskExit::Failed(TaskErrorKind::Transient))
            && matches!(
                self.spec.restart_policy,
                RestartPolicy::Transient { max_restarts }
                    if self.restart_count >= max_restarts
            )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub completed: u32,
    pub cancelled: u32,
    pub failed: u32,
    pub panicked: u32,
    pub aborted: u32,
    pub restarted: u32,
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
    task_specs: BTreeMap<TokioTaskId, TaskSpec>,
    registered: BTreeSet<TaskId>,
    scoped_cancellations: BTreeMap<TaskId, Cancellation>,
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
            task_specs: BTreeMap::new(),
            registered: BTreeSet::new(),
            scoped_cancellations: BTreeMap::new(),
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
        let task_spec = spec.clone();
        let task_handle = self.tasks.spawn(async move {
            let spec_for_completion = spec.clone();
            let exit = match future.await {
                Ok(()) => TaskExit::Completed,
                Err(error) => error.into(),
            };
            TaskCompletion {
                spec: spec_for_completion,
                exit,
                restart_count: 0,
            }
        });
        self.task_specs.insert(task_handle.id(), task_spec);
        Ok(())
    }

    /// 注册一个可以单独取消的 task；全局 shutdown 仍会取消它。
    pub fn spawn_scoped<F>(
        &mut self,
        spec: TaskSpec,
        factory: F,
    ) -> Result<Cancellation, SupervisorError>
    where
        F: FnOnce(Cancellation) -> TaskFuture + Send + 'static,
    {
        if !self.registered.insert(spec.id.clone()) {
            return Err(SupervisorError::DuplicateTask(spec.id));
        }
        let task_id = spec.id.clone();
        let task_cancellation = Cancellation::new();
        let future = factory(task_cancellation.clone());
        let supervisor_cancellation = self.cancellation.clone();
        let task_spec = spec.clone();
        self.scoped_cancellations
            .insert(task_id.clone(), task_cancellation.clone());
        let task_handle = self.tasks.spawn(async move {
            let result = tokio::select! {
                result = future => result,
                _ = supervisor_cancellation.cancelled() => Err(TaskError::Cancelled),
                _ = task_cancellation.cancelled() => Err(TaskError::Cancelled),
            };
            let spec_for_completion = spec.clone();
            let exit = match result {
                Ok(()) => TaskExit::Completed,
                Err(error) => error.into(),
            };
            TaskCompletion {
                spec: spec_for_completion,
                exit,
                restart_count: 0,
            }
        });
        self.task_specs.insert(task_handle.id(), task_spec);
        Ok(self
            .scoped_cancellations
            .get(&task_id)
            .cloned()
            .expect("scoped task cancellation must be registered"))
    }

    /// 注册一个可以单独取消且支持瞬时失败有界重试的 task。
    pub fn spawn_scoped_with_factory<F>(
        &mut self,
        spec: TaskSpec,
        factory: F,
    ) -> Result<Cancellation, SupervisorError>
    where
        F: Fn(Cancellation) -> TaskFuture + Send + Sync + 'static,
    {
        if !self.registered.insert(spec.id.clone()) {
            return Err(SupervisorError::DuplicateTask(spec.id));
        }
        let task_id = spec.id.clone();
        let task_spec = spec.clone();
        let task_cancellation = Cancellation::new();
        let supervisor_cancellation = self.cancellation.clone();
        let factory: Arc<dyn Fn(Cancellation) -> TaskFuture + Send + Sync> = Arc::new(factory);
        self.scoped_cancellations
            .insert(task_id.clone(), task_cancellation.clone());
        let task_cancellation_for_task = task_cancellation.clone();
        let task_handle = self.tasks.spawn(async move {
            let mut restart_count = 0;
            let exit = loop {
                let result = tokio::select! {
                    result = (factory)(task_cancellation_for_task.clone()) => result,
                    _ = supervisor_cancellation.cancelled() => Err(TaskError::Cancelled),
                    _ = task_cancellation_for_task.cancelled() => Err(TaskError::Cancelled),
                };
                let exit = match result {
                    Ok(()) => TaskExit::Completed,
                    Err(error) => error.into(),
                };
                if !should_restart_scoped(
                    &spec,
                    &exit,
                    restart_count,
                    &supervisor_cancellation,
                    &task_cancellation_for_task,
                ) {
                    break exit;
                }
                restart_count += 1;
                let delay = restart_backoff(restart_count);
                tokio::select! {
                    _ = supervisor_cancellation.cancelled() => break TaskExit::Cancelled,
                    _ = task_cancellation_for_task.cancelled() => break TaskExit::Cancelled,
                    _ = tokio::time::sleep(delay) => {}
                }
            };
            TaskCompletion {
                spec,
                exit,
                restart_count,
            }
        });
        self.task_specs.insert(task_handle.id(), task_spec);
        Ok(task_cancellation)
    }

    /// 注册一个可重建的 task；仅瞬时失败会按策略有界重试。
    pub fn spawn_with_factory<F>(
        &mut self,
        spec: TaskSpec,
        factory: F,
    ) -> Result<(), SupervisorError>
    where
        F: Fn() -> TaskFuture + Send + Sync + 'static,
    {
        if !self.registered.insert(spec.id.clone()) {
            return Err(SupervisorError::DuplicateTask(spec.id));
        }
        let cancellation = self.cancellation.clone();
        let task_spec = spec.clone();
        let factory: Arc<dyn Fn() -> TaskFuture + Send + Sync> = Arc::new(factory);
        let task_handle = self.tasks.spawn(async move {
            let mut restart_count = 0;
            let exit = loop {
                let result = tokio::select! {
                    result = (factory)() => result,
                    _ = cancellation.cancelled() => Err(TaskError::Cancelled),
                };
                let exit = match result {
                    Ok(()) => TaskExit::Completed,
                    Err(error) => error.into(),
                };
                if !should_restart(&spec, &exit, restart_count, &cancellation) {
                    break exit;
                }
                restart_count += 1;
                let delay = restart_backoff(restart_count);
                tokio::select! {
                    _ = cancellation.cancelled() => break TaskExit::Cancelled,
                    _ = tokio::time::sleep(delay) => {}
                }
            };
            TaskCompletion {
                spec,
                exit,
                restart_count,
            }
        });
        self.task_specs.insert(task_handle.id(), task_spec);
        Ok(())
    }

    /// 等待一个 task 结束，并从注册表移除它。
    pub async fn join_next(&mut self) -> Option<TaskCompletion> {
        let result = self.tasks.join_next_with_id().await?;
        match result {
            Ok((task_id, completion)) => {
                let registered_id = self
                    .task_specs
                    .remove(&task_id)
                    .map(|spec| spec.id)
                    .unwrap_or_else(|| completion.spec.id.clone());
                self.registered.remove(&registered_id);
                self.scoped_cancellations.remove(&registered_id);
                Some(completion)
            }
            Err(error) => {
                let task_id = error.id();
                let spec = self.task_specs.remove(&task_id).unwrap_or_else(|| {
                    TaskSpec::new(
                        format!("join-error.{task_id}"),
                        "runtime",
                        FaultLevel::Fatal,
                        RestartPolicy::Never,
                    )
                    .expect("tokio task id must produce a valid fallback task spec")
                });
                self.registered.remove(&spec.id);
                self.scoped_cancellations.remove(&spec.id);
                Some(TaskCompletion {
                    spec,
                    exit: if error.is_panic() {
                        TaskExit::Panicked
                    } else {
                        TaskExit::Cancelled
                    },
                    restart_count: 0,
                })
            }
        }
    }

    pub fn cancel_task(&self, id: &TaskId, reason: CancelReason) -> bool {
        let Some(cancellation) = self.scoped_cancellations.get(id) else {
            return false;
        };
        cancellation.cancel(reason);
        true
    }

    /// 触发 shutdown 并在截止时间前回收全部 task；超时后 abort 剩余 task。
    pub async fn shutdown(&mut self, clock: &dyn Clock, deadline: Deadline) -> ShutdownReport {
        self.cancellation.cancel(CancelReason::Shutdown);
        let mut report = ShutdownReport::default();

        while !self.tasks.is_empty() {
            tokio::select! {
                completion = self.join_next() => {
                    if let Some(completion) = completion {
                        report.restarted += completion.restart_count;
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

fn should_restart(
    spec: &TaskSpec,
    exit: &TaskExit,
    restart_count: u32,
    cancellation: &Cancellation,
) -> bool {
    !cancellation.is_cancelled()
        && matches!(exit, TaskExit::Failed(TaskErrorKind::Transient))
        && matches!(
            spec.restart_policy,
            RestartPolicy::Transient { max_restarts } if restart_count < max_restarts
        )
}

fn should_restart_scoped(
    spec: &TaskSpec,
    exit: &TaskExit,
    restart_count: u32,
    supervisor_cancellation: &Cancellation,
    task_cancellation: &Cancellation,
) -> bool {
    !supervisor_cancellation.is_cancelled()
        && !task_cancellation.is_cancelled()
        && matches!(exit, TaskExit::Failed(TaskErrorKind::Transient))
        && matches!(
            spec.restart_policy,
            RestartPolicy::Transient { max_restarts } if restart_count < max_restarts
        )
}

fn restart_backoff(restart_count: u32) -> Duration {
    let exponent = restart_count.saturating_sub(1).min(10);
    Duration::from_millis(1_u64 << exponent)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };
    use std::time::{Duration, Instant, SystemTime};

    use crate::dns::{CancelReason, Cancellation, Deadline};
    use crate::ports::effects::Clock;
    use crate::ports::testing::FakeClock;
    use crate::runtime::SystemClock;

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
    async fn panic_completion_is_attributed_to_the_panicking_task() {
        let mut supervisor = Supervisor::new();
        let sibling_cancellation = supervisor
            .spawn_scoped(spec("a.sibling"), |cancellation| {
                Box::pin(async move {
                    cancellation.cancelled().await;
                    Err(TaskError::Cancelled)
                })
            })
            .unwrap();
        supervisor
            .spawn(spec("z.panic"), Box::pin(async { panic!("test panic") }))
            .unwrap();

        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.spec.id.as_str(), "z.panic");
        assert_eq!(completion.spec.component, "test");
        assert_eq!(completion.spec.fault_level, FaultLevel::Degraded);
        assert_eq!(completion.exit, TaskExit::Panicked);
        assert_eq!(supervisor.task_count(), 1);

        sibling_cancellation.cancel(CancelReason::Shutdown);
        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.spec.id.as_str(), "a.sibling");
        assert_eq!(completion.exit, TaskExit::Cancelled);
        assert_eq!(supervisor.task_count(), 0);
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
    async fn scoped_task_cancellation_does_not_cancel_sibling_or_supervisor() {
        let mut supervisor = Supervisor::new();
        let first = supervisor
            .spawn_scoped(spec("first"), |cancellation| {
                Box::pin(async move {
                    cancellation.cancelled().await;
                    Err(TaskError::Cancelled)
                })
            })
            .unwrap();
        let _second = supervisor
            .spawn_scoped(spec("second"), |cancellation| {
                Box::pin(async move {
                    cancellation.cancelled().await;
                    Err(TaskError::Cancelled)
                })
            })
            .unwrap();

        first.cancel(CancelReason::Shutdown);
        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.spec.id.as_str(), "first");
        assert_eq!(completion.exit, TaskExit::Cancelled);
        assert_eq!(supervisor.task_count(), 1);
        assert!(!supervisor.cancellation().is_cancelled());
        assert!(!supervisor.cancel_task(&completion.spec.id, CancelReason::Shutdown));

        let second_id = TaskSpec::new("second", "test", FaultLevel::Degraded, RestartPolicy::Never)
            .unwrap()
            .id;
        assert!(supervisor.cancel_task(&second_id, CancelReason::Shutdown));
        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.spec.id.as_str(), "second");
        assert_eq!(completion.exit, TaskExit::Cancelled);
        assert_eq!(supervisor.task_count(), 0);
    }

    #[tokio::test]
    async fn scoped_factory_restarts_transient_failures_and_keeps_one_cancellation_scope() {
        let mut supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let factory_attempts = Arc::clone(&attempts);
        let cancellation = supervisor
            .spawn_scoped_with_factory(
                TaskSpec::new(
                    "scoped-restartable",
                    "test",
                    FaultLevel::FatalEndpoint,
                    RestartPolicy::Transient { max_restarts: 2 },
                )
                .unwrap(),
                move |_cancellation| {
                    let attempt = factory_attempts.fetch_add(1, Ordering::AcqRel);
                    Box::pin(async move {
                        if attempt < 2 {
                            Err(TaskError::Transient)
                        } else {
                            Ok(())
                        }
                    })
                },
            )
            .unwrap();

        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.exit, TaskExit::Completed);
        assert_eq!(completion.restart_count, 2);
        assert_eq!(attempts.load(Ordering::Acquire), 3);
        assert!(!cancellation.is_cancelled());
        assert_eq!(supervisor.task_count(), 0);
    }

    #[tokio::test]
    async fn global_shutdown_cancels_scoped_tasks() {
        let mut supervisor = Supervisor::new();
        let _cancellation = supervisor
            .spawn_scoped(spec("scoped"), |cancellation| {
                Box::pin(async move {
                    cancellation.cancelled().await;
                    Err(TaskError::Cancelled)
                })
            })
            .unwrap();

        let report = supervisor
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(1)),
            )
            .await;

        assert_eq!(report.cancelled, 1);
        assert!(!report.deadline_expired);
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

    #[tokio::test]
    async fn factory_restarts_transient_failures_within_the_configured_bound() {
        let mut supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let factory_attempts = Arc::clone(&attempts);
        supervisor
            .spawn_with_factory(
                TaskSpec::new(
                    "restartable",
                    "test",
                    FaultLevel::Degraded,
                    RestartPolicy::Transient { max_restarts: 2 },
                )
                .unwrap(),
                move || -> super::TaskFuture {
                    let attempt = factory_attempts.fetch_add(1, Ordering::AcqRel);
                    Box::pin(async move {
                        if attempt < 2 {
                            Err(TaskError::Transient)
                        } else {
                            Ok(())
                        }
                    })
                },
            )
            .unwrap();

        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.exit, TaskExit::Completed);
        assert_eq!(completion.restart_count, 2);
        assert_eq!(attempts.load(Ordering::Acquire), 3);
        assert_eq!(supervisor.task_count(), 0);
    }

    #[tokio::test]
    async fn factory_reports_transient_failure_after_restart_bound_is_exhausted() {
        let mut supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let factory_attempts = Arc::clone(&attempts);
        supervisor
            .spawn_with_factory(
                TaskSpec::new(
                    "bounded",
                    "test",
                    FaultLevel::FatalEndpoint,
                    RestartPolicy::Transient { max_restarts: 1 },
                )
                .unwrap(),
                move || -> super::TaskFuture {
                    factory_attempts.fetch_add(1, Ordering::AcqRel);
                    Box::pin(async { Err(TaskError::Transient) })
                },
            )
            .unwrap();

        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.exit, TaskExit::Failed(TaskErrorKind::Transient));
        assert_eq!(completion.restart_count, 1);
        assert!(completion.restart_exhausted());
        assert_eq!(attempts.load(Ordering::Acquire), 2);
        assert_eq!(supervisor.task_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_report_counts_restarts_before_cooperative_cancellation() {
        let mut supervisor = Supervisor::new();
        let cancellation = supervisor.cancellation();
        let attempts = Arc::new(AtomicU32::new(0));
        let factory_attempts = Arc::clone(&attempts);
        supervisor
            .spawn_with_factory(
                TaskSpec::new(
                    "restart-then-stop",
                    "test",
                    FaultLevel::Degraded,
                    RestartPolicy::Transient { max_restarts: 2 },
                )
                .unwrap(),
                move || -> super::TaskFuture {
                    let attempt = factory_attempts.fetch_add(1, Ordering::AcqRel);
                    if attempt < 2 {
                        Box::pin(async { Err(TaskError::Transient) })
                    } else {
                        let cancellation = cancellation.clone();
                        Box::pin(async move {
                            cancellation.cancelled().await;
                            Err(TaskError::Cancelled)
                        })
                    }
                },
            )
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::Acquire) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let clock = FakeClock::new(Instant::now(), SystemTime::UNIX_EPOCH);
        let report = supervisor
            .shutdown(
                &clock,
                Deadline::new(clock.monotonic_now() + Duration::from_secs(1)),
            )
            .await;

        assert_eq!(report.restarted, 2);
        assert_eq!(report.cancelled, 1);
        assert_eq!(attempts.load(Ordering::Acquire), 3);
    }

    async fn cooperative_task(cancellation: Cancellation) -> Result<(), TaskError> {
        cancellation.cancelled().await;
        Err(TaskError::Cancelled)
    }
}
