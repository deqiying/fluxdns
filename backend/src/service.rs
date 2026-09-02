//! Application 使用的 DNS service task 编排。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::task::JoinSet;

use crate::config::BindTransport;
use crate::config::resolve::ConfigId;
use crate::dns::{
    CacheCompatibilityKey, CancelReason, Cancellation, Deadline, DispatchError, DnsCore,
    RuntimeRevision, TransportCapabilities, TransportClass, dispatch_inbound,
};
use crate::ports::PortErrorClass;
use crate::ports::effects::SocketFactory;
use crate::ports::effects::{ActivatedSocketHandle, Clock};
use crate::ports::inbound::InboundAdapter;
use crate::runtime::{
    ActivationError, ActiveRuntime, AdmissionError, BindError, BoundEndpointHandle,
    BoundListenerSet, FaultLevel, PreparedRuntime, RefreshedResourceSnapshot,
    ResourceRefreshCoordinatorError, RestartPolicy, RuntimeCoordinator, RuntimeReuseError,
    ShutdownReport, Supervisor, SupervisorError, SystemClock, TaskCompletion, TaskError,
    TaskErrorKind, TaskExit, TaskSpec, bind_prepared,
};
use crate::transport::doh::{DohAdapter, DohAdapterError, DohSession, DohSessionEvent};
use crate::transport::{
    DEFAULT_REQUEST_TIMEOUT, TcpAdapter, TcpAdapterError, TcpSession, UdpAdapter, UdpAdapterError,
};

#[derive(Debug, Error)]
pub enum ServiceStartError {
    #[error("active runtime snapshot is missing its DNS core")]
    MissingDnsCore,
    #[error("could not obtain active listener handles: {class} ({operation})")]
    ListenerHandles {
        class: &'static str,
        operation: &'static str,
    },
    #[error("endpoint {index} could not create {kind} adapter: {reason}")]
    Endpoint {
        index: usize,
        kind: &'static str,
        reason: String,
    },
    #[error("could not register service task: {0}")]
    Task(#[source] SupervisorError),
}

#[derive(Debug, Error)]
pub enum ServiceReloadError {
    #[error("runtime reload bind failed: {0}")]
    Bind(#[source] BindError),
    #[error("runtime reload activation failed: {0}")]
    Activation(#[source] ActivationError),
    #[error("runtime reload listener reuse failed: {0}")]
    Reuse(#[source] RuntimeReuseError),
    #[error("active runtime snapshot is missing its DNS core")]
    MissingDnsCore,
    #[error(
        "runtime reload revision must increment by one: expected {expected:?}, actual {actual:?}"
    )]
    InvalidRevision {
        expected: RuntimeRevision,
        actual: RuntimeRevision,
    },
    #[error("runtime reload listener preparation failed: {0}")]
    Endpoint(#[source] ServiceStartError),
    #[error("runtime reload task registration failed: {0}")]
    Task(#[source] SupervisorError),
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("shutdown signal could not be installed")]
    Signal,
    #[error("service shutdown deadline expired")]
    ShutdownDeadline,
    #[error("service task {task_id} ({component}) failed at {fault_level:?}: {exit:?}")]
    TaskFailure {
        task_id: String,
        component: &'static str,
        fault_level: FaultLevel,
        exit: TaskExit,
    },
}

const RESOURCE_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const TRANSPORT_RESTART_LIMIT: u32 = 3;

type ServiceReloadFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ServiceError>> + 'a>>;

/// 已绑定 listener 的 DNS service；所有 receive loop 都由同一个 Supervisor 持有。
pub struct DnsService {
    runtime: Arc<ActiveRuntime>,
    coordinator: Arc<RuntimeCoordinator>,
    supervisor: Supervisor,
    transport_cancellations: Vec<Cancellation>,
    resource_tasks: Vec<ResourceTask>,
    request_timeout: Duration,
}

#[derive(Clone)]
struct ResourceTask {
    resource: ConfigId,
    cancellation: Cancellation,
}

impl DnsService {
    pub fn start(
        runtime: Arc<ActiveRuntime>,
        core: Arc<dyn DnsCore>,
        request_timeout: Duration,
    ) -> Result<Self, ServiceStartError> {
        let coordinator = Arc::new(RuntimeCoordinator::from_active(Arc::clone(&runtime)));
        Self::start_with_coordinator(coordinator, core, request_timeout)
    }

    pub fn start_with_coordinator(
        coordinator: Arc<RuntimeCoordinator>,
        core: Arc<dyn DnsCore>,
        request_timeout: Duration,
    ) -> Result<Self, ServiceStartError> {
        let runtime = coordinator.load();
        let mut supervisor = Supervisor::new();
        let transport_plans = prepare_transport_plans(
            runtime.listeners(),
            runtime.snapshot().config(),
            runtime.revision(),
            request_timeout,
        )?;
        let transport_cancellations = spawn_transport_plans(
            &mut supervisor,
            transport_plans,
            Arc::clone(&core),
            Arc::clone(&runtime),
        )?;

        let resource_tasks = spawn_resource_tasks(
            &mut supervisor,
            Arc::clone(&coordinator),
            runtime.revision(),
            runtime.resource_worker_ids(),
        )?;

        Ok(Self {
            runtime,
            coordinator,
            supervisor,
            transport_cancellations,
            resource_tasks,
            request_timeout,
        })
    }

    pub fn with_default_timeout(
        runtime: Arc<ActiveRuntime>,
        core: Arc<dyn DnsCore>,
    ) -> Result<Self, ServiceStartError> {
        Self::start(runtime, core, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn with_default_timeout_from_runtime(
        runtime: Arc<ActiveRuntime>,
    ) -> Result<Self, ServiceStartError> {
        let core = runtime
            .snapshot()
            .dns_core()
            .ok_or(ServiceStartError::MissingDnsCore)?;
        Self::with_default_timeout(runtime, core)
    }

    pub fn with_default_timeout_from_coordinator(
        coordinator: Arc<RuntimeCoordinator>,
    ) -> Result<Self, ServiceStartError> {
        let runtime = coordinator.load();
        let core = runtime
            .snapshot()
            .dns_core()
            .ok_or(ServiceStartError::MissingDnsCore)?;
        Self::start_with_coordinator(coordinator, core, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn runtime(&self) -> &Arc<ActiveRuntime> {
        &self.runtime
    }

    pub fn coordinator(&self) -> &Arc<RuntimeCoordinator> {
        &self.coordinator
    }

    pub fn task_count(&self) -> usize {
        self.supervisor.task_count()
    }

    pub fn transport_task_count(&self) -> usize {
        self.transport_cancellations.len()
    }

    pub fn cancel_transport_tasks(&self) {
        for cancellation in &self.transport_cancellations {
            cancellation.cancel(CancelReason::Shutdown);
        }
    }

    pub fn resource_task_count(&self) -> usize {
        self.resource_tasks.len()
    }

    pub fn cancel_resource_tasks(&self) {
        for task in &self.resource_tasks {
            task.cancellation.cancel(CancelReason::Shutdown);
        }
    }

    fn reconcile_resource_tasks(
        &mut self,
        runtime: &Arc<ActiveRuntime>,
    ) -> Result<Vec<ResourceTask>, ServiceStartError> {
        let resources = runtime.resource_worker_ids();
        let previous = self.resource_tasks.clone();
        let mut next = Vec::with_capacity(resources.len());
        let mut spawned = Vec::new();
        for (index, resource) in resources.iter().cloned().enumerate() {
            if let Some(existing) = previous.iter().find(|task| task.resource == resource) {
                next.push(existing.clone());
                continue;
            }
            match spawn_resource_task(
                &mut self.supervisor,
                Arc::clone(&self.coordinator),
                runtime.revision(),
                index,
                resource,
            ) {
                Ok(task) => {
                    spawned.push(task.clone());
                    next.push(task);
                }
                Err(error) => {
                    for task in spawned {
                        task.cancellation.cancel(CancelReason::Shutdown);
                    }
                    return Err(error);
                }
            }
        }
        for task in &previous {
            if !resources.contains(&task.resource) {
                task.cancellation.cancel(CancelReason::Shutdown);
            }
        }
        Ok(next)
    }

    /// 绑定并切换一个新 Runtime，同时重建 UDP/TCP/DoH listener task。
    ///
    /// 资源 refresh task 会按新 Runtime 的 worker ID 集合重建，旧集合在新 task
    /// 注册成功后通过 scoped cancellation 退出。
    pub async fn reload_prepared(
        &mut self,
        prepared: PreparedRuntime,
        factory: &dyn SocketFactory,
        deadline: Deadline,
        cancellation: Cancellation,
    ) -> Result<Arc<ActiveRuntime>, ServiceReloadError> {
        let expected = self.coordinator.current_revision();
        let actual = prepared.snapshot().revision();
        let expected_next = expected
            .0
            .checked_add(1)
            .map(RuntimeRevision)
            .ok_or(ServiceReloadError::InvalidRevision { expected, actual })?;
        if actual != expected_next {
            return Err(ServiceReloadError::InvalidRevision { expected, actual });
        }
        if self.runtime.bind_plan() == prepared.bind_plan() {
            let transport_plans = prepare_transport_plans(
                self.runtime.listeners(),
                prepared.snapshot().config(),
                actual,
                self.request_timeout,
            )
            .map_err(ServiceReloadError::Endpoint)?;
            let core = prepared
                .snapshot()
                .dns_core()
                .ok_or(ServiceReloadError::MissingDnsCore)?;
            let runtime = self
                .coordinator
                .activate_prepared_reusing_listeners(expected, prepared)
                .await
                .map_err(ServiceReloadError::Reuse)?;
            let transport_cancellations = spawn_transport_plans(
                &mut self.supervisor,
                transport_plans,
                core,
                Arc::clone(&runtime),
            )
            .map_err(map_reload_spawn_error)?;
            let resource_tasks = self
                .reconcile_resource_tasks(&runtime)
                .map_err(map_reload_spawn_error)?;
            self.cancel_transport_tasks();
            self.transport_cancellations = transport_cancellations;
            self.resource_tasks = resource_tasks;
            self.runtime = Arc::clone(&runtime);
            return Ok(runtime);
        }
        let candidate = bind_prepared(prepared, factory, deadline, &cancellation)
            .await
            .map_err(ServiceReloadError::Bind)?;
        let transport_plans = prepare_transport_plans(
            candidate.listeners(),
            candidate.snapshot().config(),
            candidate.revision(),
            self.request_timeout,
        )
        .map_err(ServiceReloadError::Endpoint)?;
        let core = candidate
            .snapshot()
            .dns_core()
            .ok_or(ServiceReloadError::MissingDnsCore)?;

        self.coordinator
            .compare_and_activate_serialized(expected, candidate)
            .await
            .map_err(ServiceReloadError::Activation)?;
        let runtime = self.coordinator.load();
        let transport_cancellations = spawn_transport_plans(
            &mut self.supervisor,
            transport_plans,
            core,
            Arc::clone(&runtime),
        )
        .map_err(map_reload_spawn_error)?;
        let resource_tasks = self
            .reconcile_resource_tasks(&runtime)
            .map_err(map_reload_spawn_error)?;

        self.cancel_transport_tasks();
        self.transport_cancellations = transport_cancellations;
        self.resource_tasks = resource_tasks;
        self.runtime = Arc::clone(&runtime);
        Ok(runtime)
    }

    pub async fn shutdown(
        &mut self,
        clock: &dyn Clock,
        deadline: crate::dns::Deadline,
    ) -> ShutdownReport {
        self.runtime.begin_drain();
        self.cancel_transport_tasks();
        self.cancel_resource_tasks();
        let mut report = self.supervisor.shutdown(clock, deadline).await;
        if !self.coordinator.shutdown_finalizers(deadline).await {
            report.deadline_expired = true;
        }
        report
    }

    /// 等待 Ctrl-C 后执行有界 graceful shutdown。
    pub async fn wait_for_ctrl_c(
        &mut self,
        grace_period: Duration,
    ) -> Result<ShutdownReport, ServiceError> {
        self.wait_for_ctrl_c_with_reload(grace_period, Duration::from_secs(86_400), |_service| {
            Box::pin(async { Ok(()) })
        })
        .await
    }

    /// 等待 Ctrl-C、受管 task 故障或配置变更轮询回调。
    ///
    /// 回调只负责决定是否执行一次 reload；配置错误应由调用方记录并吞掉，
    /// 不应因为一次坏配置把当前仍可用的 Runtime 变成故障。
    pub(crate) async fn wait_for_ctrl_c_with_reload<F>(
        &mut self,
        grace_period: Duration,
        poll_interval: Duration,
        mut on_poll: F,
    ) -> Result<ShutdownReport, ServiceError>
    where
        F: for<'a> FnMut(&'a mut DnsService) -> ServiceReloadFuture<'a>,
    {
        if poll_interval.is_zero() {
            return Err(ServiceError::Signal);
        }
        let signal = tokio::signal::ctrl_c();
        tokio::pin!(signal);
        let mut poll = tokio::time::interval(poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                result = &mut signal => {
                    result.map_err(|_| ServiceError::Signal)?;
                    let deadline = crate::dns::Deadline::new(Instant::now() + grace_period);
                    let report = self.shutdown(&SystemClock::new(), deadline).await;
                    if report.deadline_expired {
                        return Err(ServiceError::ShutdownDeadline);
                    }
                    return Ok(report);
                }
                completion = self.supervisor.join_next() => {
                    let Some(completion) = completion else {
                        return Err(ServiceError::TaskFailure {
                            task_id: "supervisor".to_owned(),
                            component: "runtime",
                            fault_level: FaultLevel::Fatal,
                            exit: TaskExit::Panicked,
                        });
                    };
                    if let Some(error) = task_failure(&completion) {
                        self.runtime.begin_drain();
                        return Err(error);
                    }
                }
                _ = poll.tick() => {
                    on_poll(self).await?;
                }
            }
        }
    }
}

fn map_reload_spawn_error(error: ServiceStartError) -> ServiceReloadError {
    match error {
        ServiceStartError::Task(source) => ServiceReloadError::Task(source),
        other => ServiceReloadError::Endpoint(other),
    }
}

enum TransportTaskPlan {
    Udp { index: usize, adapter: UdpAdapter },
    Tcp { index: usize, adapter: TcpAdapter },
    Doh { index: usize, adapter: DohAdapter },
}

fn prepare_transport_plans(
    listeners: &BoundListenerSet,
    config: &crate::config::resolve::ResolvedConfig,
    revision: RuntimeRevision,
    request_timeout: Duration,
) -> Result<Vec<TransportTaskPlan>, ServiceStartError> {
    let endpoints =
        listeners
            .endpoint_handles()
            .map_err(|error| ServiceStartError::ListenerHandles {
                class: error.class().as_str(),
                operation: error.operation(),
            })?;
    endpoints
        .into_iter()
        .enumerate()
        .map(|(index, endpoint)| {
            let BoundEndpointHandle { entry, socket } = endpoint;
            if entry.transport == BindTransport::Doh {
                let adapter = DohAdapter::from_endpoint(
                    BoundEndpointHandle { entry, socket },
                    config,
                    revision,
                    capabilities(TransportClass::Multiplexed),
                    request_timeout,
                )
                .map_err(|reason| ServiceStartError::Endpoint {
                    index,
                    kind: "DoH",
                    reason: reason.to_string(),
                })?;
                return Ok(TransportTaskPlan::Doh { index, adapter });
            }
            match socket {
                ActivatedSocketHandle::Udp(socket) => {
                    let adapter = UdpAdapter::from_endpoint(
                        BoundEndpointHandle {
                            entry,
                            socket: ActivatedSocketHandle::Udp(socket),
                        },
                        revision,
                        capabilities(TransportClass::Datagram),
                        request_timeout,
                    )
                    .map_err(|reason| ServiceStartError::Endpoint {
                        index,
                        kind: "UDP",
                        reason: reason.to_string(),
                    })?;
                    Ok(TransportTaskPlan::Udp { index, adapter })
                }
                ActivatedSocketHandle::Tcp(listener) => {
                    let adapter = TcpAdapter::from_endpoint(
                        BoundEndpointHandle {
                            entry,
                            socket: ActivatedSocketHandle::Tcp(listener),
                        },
                        revision,
                        capabilities(TransportClass::Stream),
                        request_timeout,
                    )
                    .map_err(|reason| ServiceStartError::Endpoint {
                        index,
                        kind: "TCP",
                        reason: reason.to_string(),
                    })?;
                    Ok(TransportTaskPlan::Tcp { index, adapter })
                }
            }
        })
        .collect()
}

fn spawn_transport_plans(
    supervisor: &mut Supervisor,
    plans: Vec<TransportTaskPlan>,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
) -> Result<Vec<Cancellation>, ServiceStartError> {
    let revision = runtime.revision().0;
    let mut cancellations = Vec::with_capacity(plans.len());
    for plan in plans {
        let cancellation = match plan {
            TransportTaskPlan::Udp { index, adapter } => {
                let task_core = Arc::clone(&core);
                let task_runtime = Arc::clone(&runtime);
                spawn_transport_task(
                    supervisor,
                    format!("transport.udp.{revision}.{index}"),
                    "udp",
                    move |cancellation| {
                        service_task(
                            adapter.clone(),
                            Arc::clone(&task_core),
                            Arc::clone(&task_runtime),
                            cancellation,
                        )
                    },
                )?
            }
            TransportTaskPlan::Tcp { index, adapter } => {
                let task_core = Arc::clone(&core);
                let task_runtime = Arc::clone(&runtime);
                spawn_transport_task(
                    supervisor,
                    format!("transport.tcp.{revision}.{index}"),
                    "tcp",
                    move |cancellation| {
                        tcp_listener_task(
                            adapter.clone(),
                            Arc::clone(&task_core),
                            Arc::clone(&task_runtime),
                            cancellation,
                        )
                    },
                )?
            }
            TransportTaskPlan::Doh { index, adapter } => {
                let task_core = Arc::clone(&core);
                let task_runtime = Arc::clone(&runtime);
                spawn_transport_task(
                    supervisor,
                    format!("transport.doh.{revision}.{index}"),
                    "doh",
                    move |cancellation| {
                        doh_listener_task(
                            adapter.clone(),
                            Arc::clone(&task_core),
                            Arc::clone(&task_runtime),
                            cancellation,
                        )
                    },
                )?
            }
        };
        cancellations.push(cancellation);
    }
    Ok(cancellations)
}

fn task_failure(completion: &TaskCompletion) -> Option<ServiceError> {
    let terminal = match completion.exit {
        TaskExit::Completed | TaskExit::Cancelled => false,
        TaskExit::Panicked => true,
        TaskExit::Failed(TaskErrorKind::Fatal) => true,
        TaskExit::Failed(TaskErrorKind::Panicked) => true,
        TaskExit::Failed(TaskErrorKind::Transient) => match completion.spec.restart_policy {
            RestartPolicy::Never => true,
            RestartPolicy::Transient { .. } => completion.restart_exhausted(),
        },
    };
    if !terminal {
        return None;
    }

    let fatal_level = matches!(
        completion.spec.fault_level,
        FaultLevel::FatalCandidate | FaultLevel::FatalEndpoint | FaultLevel::Fatal
    );
    if !fatal_level && !matches!(completion.exit, TaskExit::Panicked) {
        tracing::warn!(
            event = "service_task_degraded",
            component = completion.spec.component,
            task_id = %completion.spec.id,
            exit = ?completion.exit,
            "service_task_degraded"
        );
        return None;
    }

    Some(ServiceError::TaskFailure {
        task_id: completion.spec.id.to_string(),
        component: completion.spec.component,
        fault_level: completion.spec.fault_level,
        exit: completion.exit.clone(),
    })
}

fn capabilities(class: TransportClass) -> TransportCapabilities {
    TransportCapabilities {
        class,
        cache_compatibility: CacheCompatibilityKey(1),
    }
}

fn spawn_transport_task<F>(
    supervisor: &mut Supervisor,
    task_id: String,
    component: &'static str,
    factory: F,
) -> Result<Cancellation, ServiceStartError>
where
    F: Fn(Cancellation) -> crate::runtime::TaskFuture + Send + Sync + 'static,
{
    let spec = TaskSpec::new(
        task_id,
        component,
        FaultLevel::FatalEndpoint,
        RestartPolicy::Transient {
            max_restarts: TRANSPORT_RESTART_LIMIT,
        },
    )
    .map_err(|error| ServiceStartError::Endpoint {
        index: 0,
        kind: component,
        reason: error.to_string(),
    })?;
    supervisor
        .spawn_scoped_with_factory(spec, factory)
        .map_err(ServiceStartError::Task)
}

fn spawn_resource_tasks(
    supervisor: &mut Supervisor,
    coordinator: Arc<RuntimeCoordinator>,
    revision: RuntimeRevision,
    resources: Vec<ConfigId>,
) -> Result<Vec<ResourceTask>, ServiceStartError> {
    let mut cancellations = Vec::with_capacity(resources.len());
    for (index, resource) in resources.into_iter().enumerate() {
        cancellations.push(spawn_resource_task(
            supervisor,
            Arc::clone(&coordinator),
            revision,
            index,
            resource,
        )?);
    }
    Ok(cancellations)
}

fn spawn_resource_task(
    supervisor: &mut Supervisor,
    coordinator: Arc<RuntimeCoordinator>,
    revision: RuntimeRevision,
    index: usize,
    resource: ConfigId,
) -> Result<ResourceTask, ServiceStartError> {
    let spec = TaskSpec::new(
        format!("resource.refresh.{}.{index}", revision.0),
        "resource",
        FaultLevel::Degraded,
        RestartPolicy::Never,
    )
    .map_err(|error| ServiceStartError::Endpoint {
        index,
        kind: "resource",
        reason: error.to_string(),
    })?;
    let task_coordinator = Arc::clone(&coordinator);
    let task_resource = resource.clone();
    let cancellation = supervisor
        .spawn_scoped(spec, move |cancellation| {
            resource_refresh_task(task_coordinator, task_resource, cancellation)
        })
        .map_err(ServiceStartError::Task)?;
    Ok(ResourceTask {
        resource,
        cancellation,
    })
}

fn resource_refresh_task(
    coordinator: Arc<RuntimeCoordinator>,
    resource: ConfigId,
    cancellation: Cancellation,
) -> crate::runtime::TaskFuture {
    Box::pin(async move { run_resource_refresh_loop(coordinator, resource, cancellation).await })
}

async fn run_resource_refresh_loop(
    coordinator: Arc<RuntimeCoordinator>,
    resource: ConfigId,
    cancellation: Cancellation,
) -> Result<(), TaskError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(TaskError::Cancelled);
        }
        let now = unix_seconds();
        let runtime = coordinator.load();
        let Some(decision) = runtime.resource_refresh_decision(&resource, now) else {
            return if cancellation.is_cancelled() {
                Err(TaskError::Cancelled)
            } else {
                Err(TaskError::Fatal)
            };
        };
        if decision.is_due() {
            let deadline = Deadline::new(Instant::now() + RESOURCE_REFRESH_TIMEOUT);
            match coordinator
                .refresh_resource_if_current(
                    &runtime,
                    &resource,
                    now,
                    deadline,
                    cancellation.clone(),
                )
                .await
            {
                Ok(snapshot) => tracing::info!(
                    event = "resource_refresh_published",
                    component = "resource",
                    resource = %resource.as_str(),
                    epoch = snapshot.epoch(),
                    revision = snapshot.revision(),
                    kind = match snapshot {
                        RefreshedResourceSnapshot::Hosts(_) => "hosts",
                        RefreshedResourceSnapshot::RuleSet(_) => "rule_set",
                    },
                    "resource_refresh_published"
                ),
                Err(ResourceRefreshCoordinatorError::Stale { .. }) => continue,
                Err(_error) if cancellation.is_cancelled() => {
                    runtime.shutdown_resource_refresh();
                    return Err(TaskError::Cancelled);
                }
                Err(error) => tracing::warn!(
                    event = "resource_refresh_failed",
                    component = "resource",
                    resource = %resource.as_str(),
                    error = %error,
                    "resource_refresh_failed"
                ),
            }
            continue;
        }

        let Some(next_due) = decision.next_due() else {
            runtime.shutdown_resource_refresh();
            return Err(TaskError::Cancelled);
        };
        let wait = Duration::from_secs(next_due.saturating_sub(now).max(1));
        tokio::select! {
            _ = cancellation.cancelled() => {
                runtime.shutdown_resource_refresh();
                return Err(TaskError::Cancelled);
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn service_task<A>(
    adapter: A,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
    cancellation: Cancellation,
) -> crate::runtime::TaskFuture
where
    A: InboundAdapter + 'static,
{
    Box::pin(async move { run_adapter_loop(adapter, core, runtime, cancellation).await })
}

fn tcp_listener_task(
    adapter: TcpAdapter,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
    cancellation: Cancellation,
) -> crate::runtime::TaskFuture {
    Box::pin(async move { run_tcp_listener_loop(adapter, core, runtime, cancellation).await })
}

fn doh_listener_task(
    adapter: DohAdapter,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
    cancellation: Cancellation,
) -> crate::runtime::TaskFuture {
    Box::pin(async move { run_doh_listener_loop(adapter, core, runtime, cancellation).await })
}

async fn run_tcp_listener_loop(
    adapter: TcpAdapter,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
    cancellation: Cancellation,
) -> Result<(), TaskError> {
    let mut sessions = JoinSet::new();
    let session_cancellation = Cancellation::new();
    let mut listener_failure = None;

    loop {
        tokio::select! {
            _ = cancellation.cancelled() => {
                session_cancellation.cancel(CancelReason::Shutdown);
                break;
            }
            joined = sessions.join_next(), if !sessions.is_empty() => {
                observe_tcp_session(joined);
            }
            accepted = adapter.accept_session(&cancellation), if !cancellation.is_cancelled() => {
                match accepted {
                    Ok(Some(session)) => {
                        let session_core = Arc::clone(&core);
                        let session_runtime = Arc::clone(&runtime);
                        let session_cancellation = session_cancellation.clone();
                        sessions.spawn(async move {
                            run_tcp_connection(
                                session,
                                session_core,
                                session_runtime,
                                session_cancellation,
                            )
                            .await
                        });
                    }
                    Ok(None) => {
                        listener_failure = (!cancellation.is_cancelled()).then_some(TaskError::Transient);
                        session_cancellation.cancel(CancelReason::Shutdown);
                        break;
                    }
                    Err(error) if is_cancelled_error(&error, &cancellation) => {
                        session_cancellation.cancel(CancelReason::Shutdown);
                        break;
                    }
                    Err(error) => {
                        tracing::error!(
                            event = "tcp_listener_failed",
                            component = "service",
                            class = error.class().as_str(),
                            operation = error.operation(),
                        );
                        listener_failure = Some(TaskError::Transient);
                        session_cancellation.cancel(CancelReason::Shutdown);
                        break;
                    }
                }
            }
        }
    }

    session_cancellation.cancel(CancelReason::Shutdown);
    while let Some(joined) = sessions.join_next().await {
        observe_tcp_session(Some(joined));
    }

    if let Some(error) = listener_failure {
        return Err(error);
    }
    Err(TaskError::Cancelled)
}

async fn run_doh_listener_loop(
    adapter: DohAdapter,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
    cancellation: Cancellation,
) -> Result<(), TaskError> {
    let mut sessions = JoinSet::new();
    let session_cancellation = Cancellation::new();
    let mut listener_failure = None;

    loop {
        tokio::select! {
            _ = cancellation.cancelled() => {
                session_cancellation.cancel(CancelReason::Shutdown);
                break;
            }
            joined = sessions.join_next(), if !sessions.is_empty() => {
                observe_doh_session(joined);
            }
            accepted = adapter.accept_session(&cancellation), if !cancellation.is_cancelled() => {
                match accepted {
                    Ok(Some(session)) => {
                        let session_core = Arc::clone(&core);
                        let session_runtime = Arc::clone(&runtime);
                        let session_cancellation = session_cancellation.clone();
                        sessions.spawn(async move {
                            run_doh_connection(
                                session,
                                session_core,
                                session_runtime,
                                session_cancellation,
                            )
                            .await
                        });
                    }
                    Ok(None) => {
                        listener_failure = (!cancellation.is_cancelled()).then_some(TaskError::Transient);
                        session_cancellation.cancel(CancelReason::Shutdown);
                        break;
                    }
                    Err(error) if is_cancelled_error(&error, &cancellation) => {
                        session_cancellation.cancel(CancelReason::Shutdown);
                        break;
                    }
                    Err(error) => {
                        tracing::error!(
                            event = "doh_listener_failed",
                            component = "service",
                            class = error.class().as_str(),
                            operation = error.operation(),
                        );
                        listener_failure = Some(TaskError::Transient);
                        session_cancellation.cancel(CancelReason::Shutdown);
                        break;
                    }
                }
            }
        }
    }

    session_cancellation.cancel(CancelReason::Shutdown);
    while let Some(joined) = sessions.join_next().await {
        observe_doh_session(Some(joined));
    }

    if let Some(error) = listener_failure {
        return Err(error);
    }
    Err(TaskError::Cancelled)
}

fn observe_tcp_session(joined: Option<Result<Result<(), TaskError>, tokio::task::JoinError>>) {
    match joined {
        Some(Ok(Ok(()))) | None => {}
        Some(Ok(Err(TaskError::Cancelled))) => {}
        Some(Ok(Err(error))) => {
            tracing::debug!(
                event = "tcp_session_failed",
                component = "service",
                error = %error,
            );
        }
        Some(Err(error)) => {
            tracing::error!(
                event = "tcp_session_panicked",
                component = "service",
                panicked = error.is_panic(),
            );
        }
    }
}

fn observe_doh_session(joined: Option<Result<Result<(), TaskError>, tokio::task::JoinError>>) {
    match joined {
        Some(Ok(Ok(()))) | None => {}
        Some(Ok(Err(TaskError::Cancelled))) => {}
        Some(Ok(Err(error))) => {
            tracing::debug!(
                event = "doh_session_failed",
                component = "service",
                error = %error,
            );
        }
        Some(Err(error)) => {
            tracing::error!(
                event = "doh_session_panicked",
                component = "service",
                panicked = error.is_panic(),
            );
        }
    }
}

async fn run_tcp_connection(
    mut session: TcpSession,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
    cancellation: Cancellation,
) -> Result<(), TaskError> {
    loop {
        let inbound = match session.receive(&cancellation).await {
            Ok(Some(inbound)) => inbound,
            Ok(None) => {
                session.close().await;
                return if cancellation.is_cancelled() {
                    Err(TaskError::Cancelled)
                } else {
                    Ok(())
                };
            }
            Err(error) => {
                let cancelled = is_cancelled_error(&error, &cancellation);
                if !cancelled {
                    tracing::debug!(
                        event = "tcp_session_closed",
                        component = "service",
                        class = error.class().as_str(),
                        operation = error.operation(),
                    );
                }
                session.close().await;
                return if cancelled {
                    Err(TaskError::Cancelled)
                } else {
                    Ok(())
                };
            }
        };

        let guard = match runtime.try_acquire() {
            Ok(guard) => guard,
            Err(AdmissionError::Draining) => {
                let _ = inbound.response().cancel(CancelReason::Shutdown);
                session.close().await;
                return Ok(());
            }
            Err(AdmissionError::Capacity) => {
                let _ = inbound.response().cancel(CancelReason::GroupPolicy);
                session.close().await;
                return Ok(());
            }
        };
        let response_handle = inbound.response().clone();
        let result = tokio::select! {
            result = dispatch_inbound(core.as_ref(), inbound) => result,
            _ = cancellation.cancelled() => {
                let _ = response_handle.cancel(CancelReason::Shutdown);
                drop(guard);
                session.close().await;
                return Err(TaskError::Cancelled);
            }
        };
        drop(guard);

        if let Err(error) = result {
            handle_dispatch_error(error);
            session.close().await;
            return if cancellation.is_cancelled() {
                Err(TaskError::Cancelled)
            } else {
                Ok(())
            };
        }
    }
}

async fn run_doh_connection(
    mut session: DohSession,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
    cancellation: Cancellation,
) -> Result<(), TaskError> {
    loop {
        let event = match session.receive(&cancellation).await {
            Ok(event) => event,
            Err(error) => {
                let cancelled = is_cancelled_error(&error, &cancellation);
                if !cancelled {
                    tracing::debug!(
                        event = "doh_session_closed",
                        component = "service",
                        class = error.class().as_str(),
                        operation = error.operation(),
                    );
                }
                session.close().await;
                return if cancelled {
                    Err(TaskError::Cancelled)
                } else {
                    Ok(())
                };
            }
        };

        match event {
            DohSessionEvent::CleanEof => {
                session.close().await;
                return if cancellation.is_cancelled() {
                    Err(TaskError::Cancelled)
                } else {
                    Ok(())
                };
            }
            DohSessionEvent::HttpError { error, close } => {
                if let Err(write_error) =
                    session.write_http_error(error, close, &cancellation).await
                {
                    let cancelled = is_cancelled_error(&write_error, &cancellation);
                    if !cancelled {
                        tracing::debug!(
                            event = "doh_http_error_write_failed",
                            component = "service",
                            class = write_error.class().as_str(),
                            operation = write_error.operation(),
                        );
                    }
                    session.close().await;
                    return if cancelled {
                        Err(TaskError::Cancelled)
                    } else {
                        Ok(())
                    };
                }
                if close {
                    session.close().await;
                    return Ok(());
                }
            }
            DohSessionEvent::Request(inbound) => {
                let close_after_response = session.response_should_close();
                let guard = match runtime.try_acquire() {
                    Ok(guard) => guard,
                    Err(AdmissionError::Draining) => {
                        let _ = inbound.response().cancel(CancelReason::Shutdown);
                        session.close().await;
                        return Ok(());
                    }
                    Err(AdmissionError::Capacity) => {
                        let _ = inbound.response().cancel(CancelReason::GroupPolicy);
                        session.close().await;
                        return Ok(());
                    }
                };
                let response_handle = inbound.response().clone();
                let result = tokio::select! {
                    result = dispatch_inbound(core.as_ref(), inbound) => result,
                    _ = cancellation.cancelled() => {
                        let _ = response_handle.cancel(CancelReason::Shutdown);
                        drop(guard);
                        session.close().await;
                        return Err(TaskError::Cancelled);
                    }
                };
                drop(guard);

                if let Err(error) = result {
                    handle_dispatch_error(error);
                    session.close().await;
                    return if cancellation.is_cancelled() {
                        Err(TaskError::Cancelled)
                    } else {
                        Ok(())
                    };
                }
                if close_after_response {
                    session.close().await;
                    return Ok(());
                }
            }
        }
    }
}

fn is_cancelled_error(error: &crate::ports::PortError, cancellation: &Cancellation) -> bool {
    cancellation.is_cancelled() || matches!(error.class(), PortErrorClass::Cancelled(_))
}

async fn run_adapter_loop<A>(
    adapter: A,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
    cancellation: Cancellation,
) -> Result<(), TaskError>
where
    A: InboundAdapter + 'static,
{
    loop {
        let inbound = match adapter.receive(&cancellation).await {
            Ok(Some(inbound)) => inbound,
            Ok(None) => return Err(TaskError::Cancelled),
            Err(error) => {
                if cancellation.is_cancelled()
                    || matches!(error.class(), PortErrorClass::Cancelled(_))
                {
                    return Err(TaskError::Cancelled);
                }
                return Err(TaskError::Transient);
            }
        };

        let guard = match runtime.try_acquire() {
            Ok(guard) => guard,
            Err(AdmissionError::Draining) => {
                let _ = inbound.response().cancel(CancelReason::Shutdown);
                continue;
            }
            Err(AdmissionError::Capacity) => {
                let _ = inbound.response().cancel(CancelReason::GroupPolicy);
                continue;
            }
        };
        let response_handle = inbound.response().clone();
        let result = tokio::select! {
            result = dispatch_inbound(core.as_ref(), inbound) => result,
            _ = cancellation.cancelled() => {
                let _ = response_handle.cancel(CancelReason::Shutdown);
                return Err(TaskError::Cancelled);
            }
        };
        drop(guard);

        if let Err(error) = result {
            handle_dispatch_error(error);
        }
    }
}

fn handle_dispatch_error(error: DispatchError) {
    match error {
        DispatchError::Core(_) | DispatchError::Encode { .. } => {
            tracing::debug!(event = "request_not_responded", component = "service");
        }
    }
}

impl From<UdpAdapterError> for ServiceStartError {
    fn from(error: UdpAdapterError) -> Self {
        Self::Endpoint {
            index: 0,
            kind: "UDP",
            reason: error.to_string(),
        }
    }
}

impl From<TcpAdapterError> for ServiceStartError {
    fn from(error: TcpAdapterError) -> Self {
        Self::Endpoint {
            index: 0,
            kind: "TCP",
            reason: error.to_string(),
        }
    }
}

impl From<DohAdapterError> for ServiceStartError {
    fn from(error: DohAdapterError) -> Self {
        Self::Endpoint {
            index: 0,
            kind: "DoH",
            reason: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };
    use std::time::{Duration, Instant};

    use super::{ServiceError, capabilities, spawn_transport_task, task_failure};
    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::{
        CacheCompatibilityKey, CancelReason, Cancellation, Deadline, RuntimeRevision,
        TransportClass,
    };
    use crate::runtime::{
        FaultLevel, PreparedRuntime, RestartPolicy, RuntimeCoordinator, Supervisor, SystemClock,
        TaskCompletion, TaskError, TaskErrorKind, TaskExit, TaskSpec,
    };

    fn completion(
        fault_level: FaultLevel,
        restart_policy: RestartPolicy,
        exit: TaskExit,
        restart_count: u32,
    ) -> TaskCompletion {
        TaskCompletion {
            spec: TaskSpec::new("test.task", "test", fault_level, restart_policy).unwrap(),
            exit,
            restart_count,
        }
    }

    #[test]
    fn service_capabilities_are_transport_specific_and_stable() {
        assert_eq!(
            capabilities(TransportClass::Datagram),
            crate::dns::TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            }
        );
        assert_eq!(
            capabilities(TransportClass::Stream),
            crate::dns::TransportCapabilities {
                class: TransportClass::Stream,
                cache_compatibility: CacheCompatibilityKey(1),
            }
        );
        assert_eq!(
            capabilities(TransportClass::Multiplexed),
            crate::dns::TransportCapabilities {
                class: TransportClass::Multiplexed,
                cache_compatibility: CacheCompatibilityKey(1),
            }
        );
    }

    #[test]
    fn degraded_terminal_task_is_observed_without_stopping_the_service() {
        let completion = completion(
            FaultLevel::Degraded,
            RestartPolicy::Never,
            TaskExit::Failed(TaskErrorKind::Transient),
            0,
        );

        assert!(task_failure(&completion).is_none());
    }

    #[test]
    fn fatal_endpoint_task_failure_is_promoted_to_service_error() {
        let completion = completion(
            FaultLevel::FatalEndpoint,
            RestartPolicy::Never,
            TaskExit::Failed(TaskErrorKind::Transient),
            0,
        );

        assert!(matches!(
            task_failure(&completion),
            Some(ServiceError::TaskFailure {
                task_id,
                component: "test",
                fault_level: FaultLevel::FatalEndpoint,
                exit: TaskExit::Failed(TaskErrorKind::Transient),
            }) if task_id == "test.task"
        ));
    }

    #[test]
    fn bounded_restart_failure_is_only_promoted_after_exhaustion() {
        let running = completion(
            FaultLevel::FatalEndpoint,
            RestartPolicy::Transient { max_restarts: 2 },
            TaskExit::Failed(TaskErrorKind::Transient),
            1,
        );
        assert!(task_failure(&running).is_none());

        let exhausted = completion(
            FaultLevel::FatalEndpoint,
            RestartPolicy::Transient { max_restarts: 2 },
            TaskExit::Failed(TaskErrorKind::Transient),
            2,
        );
        assert!(matches!(
            task_failure(&exhausted),
            Some(ServiceError::TaskFailure { .. })
        ));
    }

    #[test]
    fn task_panic_is_fatal_even_for_a_degraded_component() {
        let completion = completion(
            FaultLevel::Degraded,
            RestartPolicy::Never,
            TaskExit::Panicked,
            0,
        );

        assert!(matches!(
            task_failure(&completion),
            Some(ServiceError::TaskFailure {
                exit: TaskExit::Panicked,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn transport_task_is_registered_with_a_scoped_cancellation() {
        let mut supervisor = Supervisor::new();
        let cancellation = spawn_transport_task(
            &mut supervisor,
            "transport.test".to_owned(),
            "test",
            |cancellation| {
                Box::pin(async move {
                    cancellation.cancelled().await;
                    Err(TaskError::Cancelled)
                })
            },
        )
        .unwrap();

        cancellation.cancel(CancelReason::Shutdown);
        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.spec.id.as_str(), "transport.test");
        assert_eq!(completion.exit, TaskExit::Cancelled);
        assert_eq!(supervisor.task_count(), 0);
    }

    #[tokio::test]
    async fn transport_task_retries_transient_failure_before_scoped_shutdown() {
        let mut supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let factory_attempts = Arc::clone(&attempts);
        let cancellation = spawn_transport_task(
            &mut supervisor,
            "transport.retry".to_owned(),
            "test",
            move |cancellation| {
                let attempt = factory_attempts.fetch_add(1, Ordering::AcqRel);
                if attempt == 0 {
                    Box::pin(async { Err(TaskError::Transient) }) as crate::runtime::TaskFuture
                } else {
                    Box::pin(async move {
                        cancellation.cancelled().await;
                        Err(TaskError::Cancelled)
                    })
                }
            },
        )
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancellation.cancel(CancelReason::Shutdown);

        let completion = supervisor.join_next().await.unwrap();
        assert_eq!(completion.spec.id.as_str(), "transport.retry");
        assert_eq!(completion.exit, TaskExit::Cancelled);
        assert_eq!(completion.restart_count, 1);
        assert_eq!(supervisor.task_count(), 0);
    }

    fn runtime_config(port: u16) -> Arc<crate::config::resolve::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("service-reload");
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 1
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {{}}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: {port}
    strategy: default
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "127.0.0.1 example.test"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: "127.0.0.1 example.test"
outbound: []
rule_set: []
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
clients: []
"#,
                port = port,
                work_path = work_path,
            ))
            .expect("service reload fixture must be valid")
            .resolved
    }

    fn resource_runtime_config(
        root: &std::path::Path,
        resource_path: &std::path::Path,
        port: u16,
        auto_update: bool,
    ) -> Arc<crate::config::resolve::ResolvedConfig> {
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 1
work:
  path: {root}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {{}}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: {port}
    strategy: default
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "127.0.0.1 fallback.test"
hosts:
  - type: file
    name: local-hosts
    format: hosts
    path: {resource_path}
    auto_update: {auto_update}
    update_interval: 60s
outbound: []
rule_set: []
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
clients: []
"#,
                root = root.display(),
                resource_path = resource_path.display(),
                port = port,
                auto_update = auto_update,
            ))
            .expect("resource service reload fixture must be valid")
            .resolved
    }

    #[tokio::test]
    async fn reload_prepared_rebinds_listener_tasks_to_the_new_runtime() {
        let base_port = 40_000 + (std::process::id() as u16 % 1_000) * 2;
        let initial = PreparedRuntime::prepare_with_policy_core(
            runtime_config(base_port),
            RuntimeRevision(1),
        )
        .unwrap();
        let factory = crate::runtime::SystemSocketFactory::new();
        let initial = crate::runtime::bind_prepared(
            initial,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(initial));
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();

        let prepared = PreparedRuntime::prepare_with_policy_core(
            runtime_config(base_port + 1),
            RuntimeRevision(2),
        )
        .unwrap();
        let active = service
            .reload_prepared(
                prepared,
                &factory,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();

        assert_eq!(active.revision(), RuntimeRevision(2));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(2));
        assert_eq!(service.runtime().revision(), RuntimeRevision(2));
        assert_eq!(service.transport_task_count(), 1);
        assert_eq!(
            active.listeners().local_addrs().unwrap()[0].port(),
            base_port + 1
        );

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await;
        assert!(!report.deadline_expired);
    }

    #[tokio::test]
    async fn reload_prepared_reuses_unchanged_listener_without_rebinding() {
        let port = 40_500 + (std::process::id() as u16 % 500);
        let initial =
            PreparedRuntime::prepare_with_policy_core(runtime_config(port), RuntimeRevision(1))
                .unwrap();
        let factory = crate::runtime::SystemSocketFactory::new();
        let initial = crate::runtime::bind_prepared(
            initial,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(initial));
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();

        let prepared =
            PreparedRuntime::prepare_with_policy_core(runtime_config(port), RuntimeRevision(2))
                .unwrap();
        let active = service
            .reload_prepared(
                prepared,
                &factory,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();

        assert_eq!(active.revision(), RuntimeRevision(2));
        assert_eq!(active.listeners().local_addrs().unwrap()[0].port(), port);
        assert_eq!(service.transport_task_count(), 1);
        assert_eq!(service.resource_task_count(), 0);

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await;
        assert!(!report.deadline_expired);
    }

    #[tokio::test]
    async fn reusing_listener_merges_published_resource_state() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-service-reuse-resource-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let resource_path = root.join("hosts.txt");
        std::fs::write(&resource_path, "192.0.2.10 old.example\n").unwrap();
        let port = 42_500 + (std::process::id() as u16 % 500);
        let factory = crate::runtime::SystemSocketFactory::new();
        let initial = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            resource_runtime_config(&root, &resource_path, port, true),
            RuntimeRevision(1),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        let initial = crate::runtime::bind_prepared(
            initial,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = RuntimeCoordinator::new(initial);

        std::fs::write(&resource_path, "192.0.2.11 new.example\n").unwrap();
        let resource = crate::config::resolve::ConfigId::new("local-hosts").unwrap();
        let refreshed = coordinator
            .refresh_resource(
                &resource,
                u64::MAX,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(refreshed.epoch(), 2);

        let prepared = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            resource_runtime_config(&root, &resource_path, port, true),
            RuntimeRevision(2),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        let active = coordinator
            .activate_prepared_reusing_listeners(RuntimeRevision(1), prepared)
            .await
            .unwrap();

        assert_eq!(active.listeners().local_addrs().unwrap()[0].port(), port);
        assert_eq!(
            active
                .snapshot()
                .resources()
                .lookup(&resource)
                .unwrap()
                .version(),
            crate::resource::ResourceVersion::new(2, 0)
        );
        active.shutdown_resource_refresh();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shutdown_closes_finalizers_from_previous_and_current_runtime() {
        let port = 41_500 + (std::process::id() as u16 % 500);
        let initial =
            PreparedRuntime::prepare_with_policy_core(runtime_config(port), RuntimeRevision(1))
                .unwrap();
        let factory = crate::runtime::SystemSocketFactory::new();
        let initial = crate::runtime::bind_prepared(
            initial,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let old_finalizer = initial.snapshot().policy_core().unwrap().finalizer_owner();
        old_finalizer
            .submit_task(std::future::pending::<()>())
            .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(initial));
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();

        let prepared =
            PreparedRuntime::prepare_with_policy_core(runtime_config(port), RuntimeRevision(2))
                .unwrap();
        let current_finalizer = prepared.snapshot().policy_core().unwrap().finalizer_owner();
        current_finalizer
            .submit_task(std::future::pending::<()>())
            .unwrap();
        service
            .reload_prepared(
                prepared,
                &factory,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();

        assert!(!old_finalizer.is_shutdown());
        assert!(!current_finalizer.is_shutdown());
        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await;
        assert!(!report.deadline_expired);
        assert!(old_finalizer.is_shutdown());
        assert!(current_finalizer.is_shutdown());
    }

    #[tokio::test]
    async fn reload_prepared_reconciles_resource_worker_tokens() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-service-resource-reload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let resource_path = root.join("hosts.txt");
        std::fs::write(&resource_path, "192.0.2.10 example.test\n").unwrap();
        let base_port = 41_000 + (std::process::id() as u16 % 1_000) * 2;
        let factory = crate::runtime::SystemSocketFactory::new();
        let initial = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            resource_runtime_config(&root, &resource_path, base_port, true),
            RuntimeRevision(1),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        let initial = crate::runtime::bind_prepared(
            initial,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(initial));
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();
        assert_eq!(service.resource_task_count(), 1);
        let old_token = service.resource_tasks[0].cancellation.clone();

        let next = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            resource_runtime_config(&root, &resource_path, base_port + 1, true),
            RuntimeRevision(2),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        service
            .reload_prepared(
                next,
                &factory,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();

        assert!(!old_token.is_cancelled());
        assert_eq!(service.resource_task_count(), 1);
        assert!(!service.resource_tasks[0].cancellation.is_cancelled());

        let next_without_resource = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            resource_runtime_config(&root, &resource_path, base_port + 2, false),
            RuntimeRevision(3),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        service
            .reload_prepared(
                next_without_resource,
                &factory,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();
        assert!(old_token.is_cancelled());
        assert_eq!(service.resource_task_count(), 0);

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await;
        assert!(!report.deadline_expired);
        let _ = std::fs::remove_dir_all(root);
    }
}
