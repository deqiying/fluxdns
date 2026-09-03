//! Application 使用的 DNS service task 编排。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::task::JoinSet;

use crate::config::resolve::ConfigId;
use crate::config::{BindTransport, ResolvedConfig};
use crate::dns::{
    CancelReason, Cancellation, CoreError, CoreOutcome, Deadline, DispatchError, DnsCore,
    DnsRequest, ResponseClass, RuntimeRevision, TransportClass, dispatch_inbound,
};
use crate::observability::TelemetryWriter;
use crate::ports::PortErrorClass;
use crate::ports::effects::SocketFactory;
use crate::ports::effects::{ActivatedSocketHandle, Clock};
use crate::ports::inbound::InboundAdapter;
use crate::ports::storage::{
    ResolveEvent, ResolveEventDisposition, ResolveEventSink, ResolveRuleSource, StatsDimension,
};
use crate::ports::telemetry::{
    CacheStatus, Component as TelemetryComponent, ComponentHealthEvent, ComponentHealthState,
    ConfiguredIdKind, HealthSink, LogSink, OutcomeClass, configured_id_from_validated,
};
use crate::runtime::{
    ActivationError, ActiveRuntime, AdmissionError, BindError, BoundEndpointHandle,
    BoundListenerSet, CacheFinalizerShutdownSummary, FaultLevel, PreparedRuntime,
    RefreshedResourceSnapshot, ResourceRefreshCoordinatorError, RestartPolicy, RuntimeCoordinator,
    RuntimeReuseError, ShutdownReport, Supervisor, SupervisorError, SystemClock, TaskCompletion,
    TaskError, TaskErrorKind, TaskExit, TaskSpec, bind_prepared,
};
use crate::storage::{
    DEFAULT_STORAGE_FLUSH_INTERVAL, DEFAULT_STORAGE_OPERATION_TIMEOUT, StatsPersistenceWorker,
    StorageRuntime, StorageServiceError, StorageServiceFlushSummary, day_utc,
};
use crate::transport::doh::{DohAdapter, DohAdapterError, DohSession, DohSessionEvent};
use crate::transport::{
    DEFAULT_REQUEST_TIMEOUT, TcpAdapter, TcpAdapterError, TcpSession, UdpAdapter, UdpAdapterError,
    transport_capabilities,
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
    #[error(
        "runtime reload changes process-owned {component} configuration and requires process restart"
    )]
    RestartRequired { component: &'static str },
}

/// 表示热重载是否需要替换网络 listener。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceReloadMode {
    ReuseListeners,
    RebindListeners,
}

/// 根据新旧配置选择 service 热重载路径。
///
/// 由进程启动阶段持有的资源当前不能原位替换；检测到相关配置变化时必须拒绝
/// 热重载，使调用方保留旧 Runtime 并提示重启进程。
fn classify_service_reload(
    current: &ResolvedConfig,
    candidate: &ResolvedConfig,
) -> Result<ServiceReloadMode, ServiceReloadError> {
    if let Some(component) = process_owned_reload_change(current, candidate) {
        return Err(ServiceReloadError::RestartRequired { component });
    }
    if current.bind_plan == candidate.bind_plan {
        Ok(ServiceReloadMode::ReuseListeners)
    } else {
        Ok(ServiceReloadMode::RebindListeners)
    }
}

/// 返回阻止当前候选热重载的进程持有配置组件。
pub(crate) fn process_owned_reload_change(
    current: &ResolvedConfig,
    candidate: &ResolvedConfig,
) -> Option<&'static str> {
    if current.database != candidate.database {
        Some("database")
    } else if current.logs != candidate.logs {
        Some("logs")
    } else if current.webui != candidate.webui {
        Some("webui")
    } else if current.dns.resolve_log != candidate.dns.resolve_log {
        Some("dns.resolve_log")
    } else {
        None
    }
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
    #[error("storage shutdown failed: {0}")]
    Storage(#[source] StorageServiceError),
    #[error("telemetry shutdown failed: {0}")]
    Telemetry(#[source] crate::ports::PortError),
}

const RESOURCE_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const TRANSPORT_RESTART_LIMIT: u32 = 3;
const MAX_CONCURRENT_STREAM_SESSIONS: usize = 1_024;
const TELEMETRY_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const TELEMETRY_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

type ServiceReloadFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ServiceError>> + 'a>>;

/// 已绑定 listener 的 DNS service；所有 receive loop 都由同一个 Supervisor 持有。
pub struct DnsService {
    runtime: Arc<ActiveRuntime>,
    coordinator: Arc<RuntimeCoordinator>,
    supervisor: Supervisor,
    transport_tasks: Vec<TransportTask>,
    resource_tasks: Vec<ResourceTask>,
    request_timeout: Duration,
    storage: Option<Arc<tokio::sync::Mutex<StorageRuntime>>>,
    stats_worker: Option<Arc<StatsPersistenceWorker>>,
    resolve_event_sink: Option<Arc<dyn ResolveEventSink>>,
    resolve_detail_drops: Arc<ResolveDetailDropCounters>,
    telemetry: Option<Arc<TelemetryWriter>>,
}

#[derive(Clone)]
struct ResourceTask {
    resource: ConfigId,
    cancellation: Cancellation,
}

/// 当前 Runtime 的 transport task 身份、逻辑 listener 归属和取消句柄。
#[derive(Clone)]
struct TransportTask {
    task_id: String,
    owner: String,
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
        Self::start_with_optional_storage(coordinator, core, request_timeout, None)
    }

    pub fn start_with_coordinator_and_storage(
        coordinator: Arc<RuntimeCoordinator>,
        core: Arc<dyn DnsCore>,
        request_timeout: Duration,
        storage: StorageRuntime,
    ) -> Result<Self, ServiceStartError> {
        Self::start_with_optional_storage(coordinator, core, request_timeout, Some(storage))
    }

    pub fn start_with_coordinator_storage_and_telemetry(
        coordinator: Arc<RuntimeCoordinator>,
        core: Arc<dyn DnsCore>,
        request_timeout: Duration,
        storage: StorageRuntime,
        telemetry: Arc<TelemetryWriter>,
    ) -> Result<Self, ServiceStartError> {
        Self::start_with_optional_storage_and_telemetry(
            coordinator,
            core,
            request_timeout,
            Some(storage),
            Some(telemetry),
        )
    }

    fn start_with_optional_storage(
        coordinator: Arc<RuntimeCoordinator>,
        core: Arc<dyn DnsCore>,
        request_timeout: Duration,
        storage: Option<StorageRuntime>,
    ) -> Result<Self, ServiceStartError> {
        Self::start_with_optional_storage_and_telemetry(
            coordinator,
            core,
            request_timeout,
            storage,
            None,
        )
    }

    fn start_with_optional_storage_and_telemetry(
        coordinator: Arc<RuntimeCoordinator>,
        core: Arc<dyn DnsCore>,
        request_timeout: Duration,
        storage: Option<StorageRuntime>,
        telemetry: Option<Arc<TelemetryWriter>>,
    ) -> Result<Self, ServiceStartError> {
        let runtime = coordinator.load();
        let mut supervisor = Supervisor::new();
        let (storage, stats_worker, resolve_event_sink) = match storage {
            Some(storage) => {
                let stats_worker = storage.stats_worker();
                let resolve_event_sink = storage.resolve_event_sink();
                let storage = Arc::new(tokio::sync::Mutex::new(storage));
                (Some(storage), Some(stats_worker), resolve_event_sink)
            }
            None => (None, None, None),
        };
        let resolve_detail_drops = Arc::new(ResolveDetailDropCounters::default());
        let core = instrumented_core(
            core,
            stats_worker.clone(),
            resolve_event_sink.clone(),
            telemetry.clone(),
            Arc::clone(&resolve_detail_drops),
        );
        if let Some(storage) = &storage {
            spawn_storage_task(&mut supervisor, Arc::clone(storage), telemetry.clone())?;
        }
        if let Some(telemetry) = &telemetry {
            publish_component_health(
                telemetry,
                TelemetryComponent::Telemetry,
                ComponentHealthState::Healthy,
                None,
            );
            if storage.is_some() {
                publish_component_health(
                    telemetry,
                    TelemetryComponent::Storage,
                    ComponentHealthState::Healthy,
                    None,
                );
            }
            spawn_telemetry_task(&mut supervisor, Arc::clone(telemetry))?;
        }
        let transport_plans = prepare_transport_plans(
            runtime.listeners(),
            runtime.snapshot().config(),
            runtime.revision(),
            request_timeout,
        )?;
        let transport_tasks = spawn_transport_plans(
            &mut supervisor,
            transport_plans,
            Arc::clone(&core),
            Arc::clone(&runtime),
        )?;
        if let Some(telemetry) = &telemetry
            && !transport_tasks.is_empty()
        {
            publish_component_health(
                telemetry,
                TelemetryComponent::Listener,
                ComponentHealthState::Healthy,
                None,
            );
        }

        let resource_worker_ids = runtime.resource_worker_ids();
        if let Some(telemetry) = &telemetry
            && !resource_worker_ids.is_empty()
        {
            publish_component_health(
                telemetry,
                TelemetryComponent::Resource,
                ComponentHealthState::Healthy,
                None,
            );
        }
        let resource_tasks = spawn_resource_tasks(
            &mut supervisor,
            Arc::clone(&coordinator),
            runtime.revision(),
            resource_worker_ids,
            telemetry.clone(),
        )?;

        Ok(Self {
            runtime,
            coordinator,
            supervisor,
            transport_tasks,
            resource_tasks,
            request_timeout,
            storage,
            stats_worker,
            resolve_event_sink,
            resolve_detail_drops,
            telemetry,
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

    pub fn with_default_timeout_from_coordinator_and_storage(
        coordinator: Arc<RuntimeCoordinator>,
        storage: StorageRuntime,
    ) -> Result<Self, ServiceStartError> {
        let runtime = coordinator.load();
        let core = runtime
            .snapshot()
            .dns_core()
            .ok_or(ServiceStartError::MissingDnsCore)?;
        Self::start_with_coordinator_and_storage(
            coordinator,
            core,
            DEFAULT_REQUEST_TIMEOUT,
            storage,
        )
    }

    pub fn with_default_timeout_from_coordinator_storage_and_telemetry(
        coordinator: Arc<RuntimeCoordinator>,
        storage: StorageRuntime,
        telemetry: Arc<TelemetryWriter>,
    ) -> Result<Self, ServiceStartError> {
        let runtime = coordinator.load();
        let core = runtime
            .snapshot()
            .dns_core()
            .ok_or(ServiceStartError::MissingDnsCore)?;
        Self::start_with_coordinator_storage_and_telemetry(
            coordinator,
            core,
            DEFAULT_REQUEST_TIMEOUT,
            storage,
            telemetry,
        )
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

    /// 返回当前 Runtime 仍可服务的 transport endpoint task 数量。
    pub fn transport_task_count(&self) -> usize {
        self.transport_tasks.len()
    }

    /// 取消当前 Runtime 的 transport task；旧 revision 的 task 已由 reload 单独取消。
    pub fn cancel_transport_tasks(&self) {
        for task in &self.transport_tasks {
            task.cancellation.cancel(CancelReason::Shutdown);
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
                self.telemetry.clone(),
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

    /// 切换一个新 Runtime，并按配置差异复用或重建 UDP/TCP/DoH listener task。
    ///
    /// 资源 refresh task 会按新 Runtime 的 worker ID 集合重建，旧集合在新 task
    /// 注册成功后通过 scoped cancellation 退出。进程级资源配置发生变化时拒绝切换，
    /// 由调用方保留旧 Runtime 并要求重启进程。
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
        let reload_mode = classify_service_reload(
            self.runtime.snapshot().config(),
            prepared.snapshot().config(),
        )?;
        if reload_mode == ServiceReloadMode::ReuseListeners {
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
            let core = self.instrument_core(core);
            let runtime = self
                .coordinator
                .activate_prepared_reusing_listeners(expected, prepared)
                .await
                .map_err(ServiceReloadError::Reuse)?;
            let transport_tasks = spawn_transport_plans(
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
            self.transport_tasks = transport_tasks;
            self.resource_tasks = resource_tasks;
            self.runtime = Arc::clone(&runtime);
            if let Some(telemetry) = &self.telemetry
                && !self.transport_tasks.is_empty()
            {
                publish_component_health(
                    telemetry,
                    TelemetryComponent::Listener,
                    ComponentHealthState::Healthy,
                    None,
                );
            }
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
        let core = self.instrument_core(core);

        self.coordinator
            .compare_and_activate_serialized(expected, candidate)
            .await
            .map_err(ServiceReloadError::Activation)?;
        let runtime = self.coordinator.load();
        let transport_tasks = spawn_transport_plans(
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
        self.transport_tasks = transport_tasks;
        self.resource_tasks = resource_tasks;
        self.runtime = Arc::clone(&runtime);
        if let Some(telemetry) = &self.telemetry
            && !self.transport_tasks.is_empty()
        {
            publish_component_health(
                telemetry,
                TelemetryComponent::Listener,
                ComponentHealthState::Healthy,
                None,
            );
        }
        Ok(runtime)
    }

    fn instrument_core(&self, core: Arc<dyn DnsCore>) -> Arc<dyn DnsCore> {
        instrumented_core(
            core,
            self.stats_worker.clone(),
            self.resolve_event_sink.clone(),
            self.telemetry.clone(),
            Arc::clone(&self.resolve_detail_drops),
        )
    }

    pub async fn shutdown(
        &mut self,
        clock: &dyn Clock,
        deadline: crate::dns::Deadline,
    ) -> Result<ShutdownReport, ServiceError> {
        self.coordinator.begin_drain();
        if let Some(telemetry) = &self.telemetry
            && !self.transport_tasks.is_empty()
        {
            publish_component_health(
                telemetry,
                TelemetryComponent::Listener,
                ComponentHealthState::Stopping,
                None,
            );
        }
        self.cancel_transport_tasks();
        self.cancel_resource_tasks();
        let mut report = self.supervisor.shutdown(clock, deadline).await;
        if !self.coordinator.wait_for_drain(deadline).await {
            report.deadline_expired = true;
        }
        let cache_summary = self.coordinator.shutdown_finalizers(deadline).await;
        if !cache_summary.completed {
            report.deadline_expired = true;
        }
        if let Some(telemetry) = &self.telemetry {
            publish_cache_shutdown_health(telemetry, cache_summary);
        }
        log_cache_shutdown_summary(cache_summary);
        let storage_error = if let Some(storage) = self.storage.take() {
            if let Some(telemetry) = &self.telemetry {
                publish_component_health(
                    telemetry,
                    TelemetryComponent::Storage,
                    ComponentHealthState::Stopping,
                    None,
                );
            }
            match storage.lock().await.shutdown(deadline).await {
                Ok(summary) => {
                    log_storage_shutdown_summary(summary);
                    None
                }
                Err(error) => Some(ServiceError::Storage(error)),
            }
        } else {
            None
        };
        let telemetry_error = self.telemetry.take().and_then(|telemetry| {
            publish_component_health(
                &telemetry,
                TelemetryComponent::Telemetry,
                ComponentHealthState::Stopping,
                None,
            );
            telemetry
                .shutdown(deadline)
                .err()
                .map(ServiceError::Telemetry)
        });
        if let Some(error) = storage_error {
            return Err(error);
        }
        if let Some(error) = telemetry_error {
            return Err(error);
        }
        Ok(report)
    }

    /// 运行期 task 失败后执行有界收尾；清理异常只记录，调用方仍返回原始 task 错误。
    async fn shutdown_after_runtime_failure(&mut self, grace_period: Duration) {
        let clock = SystemClock::new();
        let deadline = crate::dns::Deadline::new(Instant::now() + grace_period);
        match self.shutdown(&clock, deadline).await {
            Ok(report) if report.deadline_expired => {
                tracing::error!(
                    event = "runtime_failure_shutdown_timeout",
                    component = "runtime",
                    "runtime_failure_shutdown_timeout"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(
                    event = "runtime_failure_shutdown_failed",
                    component = "runtime",
                    reason = %error,
                    "runtime_failure_shutdown_failed"
                );
            }
        }
    }

    /// 等待进程终止信号后执行有界 graceful shutdown。
    pub async fn wait_for_ctrl_c(
        &mut self,
        grace_period: Duration,
    ) -> Result<ShutdownReport, ServiceError> {
        self.wait_for_ctrl_c_with_reload(grace_period, Duration::from_secs(86_400), |_service| {
            Box::pin(async { Ok(()) })
        })
        .await
    }

    /// 等待终止信号、受管 task 故障或配置变更轮询回调。
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
        let signal = wait_for_termination_signal();
        tokio::pin!(signal);
        let mut poll = tokio::time::interval(poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                result = &mut signal => {
                    result?;
                    let deadline = crate::dns::Deadline::new(Instant::now() + grace_period);
                    let clock = SystemClock::new();
                    let shutdown = self.shutdown(&clock, deadline);
                    tokio::pin!(shutdown);
                    let second_signal = wait_for_termination_signal();
                    tokio::pin!(second_signal);
                    tokio::select! {
                        result = &mut shutdown => {
                            let report = result?;
                            if report.deadline_expired {
                                return Err(ServiceError::ShutdownDeadline);
                            }
                            return Ok(report);
                        }
                        result = &mut second_signal => {
                            result?;
                            return Err(ServiceError::Signal);
                        }
                    }
                }
                completion = self.supervisor.join_next() => {
                    let Some(completion) = completion else {
                        let error = ServiceError::TaskFailure {
                            task_id: "supervisor".to_owned(),
                            component: "runtime",
                            fault_level: FaultLevel::Fatal,
                            exit: TaskExit::Panicked,
                        };
                        self.shutdown_after_runtime_failure(grace_period).await;
                        return Err(error);
                    };
                    if let Some(error) = task_failure(&completion) {
                        if is_exhausted_endpoint(&completion) {
                            match retire_current_transport_task(
                                &mut self.transport_tasks,
                                &completion,
                            ) {
                                Some(remaining) if remaining > 0 => {
                                    if let Some(telemetry) = &self.telemetry {
                                        publish_component_health(
                                            telemetry,
                                            TelemetryComponent::Listener,
                                            ComponentHealthState::Degraded,
                                            Some("listener endpoint exhausted retries"),
                                        );
                                    }
                                    tracing::warn!(
                                        event = "listener_endpoint_unavailable",
                                        component = completion.spec.component,
                                        task_id = %completion.spec.id,
                                        remaining,
                                        "listener_endpoint_unavailable"
                                    );
                                    continue;
                                }
                                None => {
                                    tracing::debug!(
                                        event = "stale_listener_failure_ignored",
                                        component = completion.spec.component,
                                        task_id = %completion.spec.id,
                                        "stale_listener_failure_ignored"
                                    );
                                    continue;
                                }
                                Some(0) => {}
                                Some(_) => unreachable!(),
                            }
                        }
                        if let Some(telemetry) = &self.telemetry {
                            publish_component_health(
                                telemetry,
                                telemetry_component_for_task(completion.spec.component),
                                ComponentHealthState::Failed,
                                Some("supervisor task failed"),
                            );
                        }
                        self.shutdown_after_runtime_failure(grace_period).await;
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

fn instrumented_core(
    core: Arc<dyn DnsCore>,
    stats_worker: Option<Arc<StatsPersistenceWorker>>,
    resolve_event_sink: Option<Arc<dyn ResolveEventSink>>,
    telemetry: Option<Arc<TelemetryWriter>>,
    resolve_detail_drops: Arc<ResolveDetailDropCounters>,
) -> Arc<dyn DnsCore> {
    if stats_worker.is_none() && resolve_event_sink.is_none() {
        return core;
    }
    Arc::new(ObservedDnsCore {
        inner: core,
        stats_worker,
        resolve_event_sink,
        telemetry,
        resolve_detail_drops,
    })
}

struct ObservedDnsCore {
    inner: Arc<dyn DnsCore>,
    stats_worker: Option<Arc<StatsPersistenceWorker>>,
    resolve_event_sink: Option<Arc<dyn ResolveEventSink>>,
    telemetry: Option<Arc<TelemetryWriter>>,
    resolve_detail_drops: Arc<ResolveDetailDropCounters>,
}

#[derive(Default)]
struct ResolveDetailDropCounters {
    queue_full: AtomicU64,
    policy: AtomicU64,
    failed: AtomicU64,
    degraded: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolveDetailHealthUpdate {
    state: ComponentHealthState,
    retry_count: u64,
    safe_reason: Option<&'static str>,
}

impl ResolveDetailDropCounters {
    /// 累计详情记录的明确丢弃，并按 2 的幂次输出限频状态事件。
    fn record(&self, disposition: ResolveEventDisposition) -> Option<ResolveDetailHealthUpdate> {
        if disposition == ResolveEventDisposition::Accepted {
            return self.recover_if_needed();
        }
        let (reason, counter) = match disposition {
            ResolveEventDisposition::DroppedQueueFull => ("queue_full", &self.queue_full),
            ResolveEventDisposition::DroppedByPolicy => ("policy", &self.policy),
            ResolveEventDisposition::Accepted | ResolveEventDisposition::Disabled => return None,
        };
        let dropped_total = counter.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        if dropped_total.is_power_of_two() {
            tracing::warn!(
                event = "resolve_detail_record_dropped",
                component = "storage",
                reason,
                dropped_total,
                "resolve_detail_record_dropped"
            );
        }
        if disposition == ResolveEventDisposition::DroppedByPolicy {
            return None;
        }
        self.degraded_update(
            dropped_total.is_power_of_two(),
            "resolve detail queue is full",
        )
    }

    /// 累计详情 sink 错误，并以同一限频规则生成 degraded health 更新。
    fn record_failure(&self) -> Option<ResolveDetailHealthUpdate> {
        let failed_total = self
            .failed
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.degraded_update(
            failed_total.is_power_of_two(),
            "resolve detail record failed",
        )
    }

    /// 首次故障和 2 的幂次累计点发布更新，避免持续故障淹没有界 telemetry 队列。
    fn degraded_update(
        &self,
        publish_counter_update: bool,
        safe_reason: &'static str,
    ) -> Option<ResolveDetailHealthUpdate> {
        let was_degraded = self.degraded.swap(true, Ordering::AcqRel);
        (!was_degraded || publish_counter_update).then(|| ResolveDetailHealthUpdate {
            state: ComponentHealthState::Degraded,
            retry_count: self.total_failures(),
            safe_reason: Some(safe_reason),
        })
    }

    /// 下一条成功接收的详情记录负责关闭本地 degraded 生命周期。
    fn recover_if_needed(&self) -> Option<ResolveDetailHealthUpdate> {
        self.degraded
            .swap(false, Ordering::AcqRel)
            .then(|| ResolveDetailHealthUpdate {
                state: ComponentHealthState::Healthy,
                retry_count: self.total_failures(),
                safe_reason: None,
            })
    }

    /// 返回队列溢出和 sink 错误的累计次数；主动策略丢弃不计入故障重试。
    fn total_failures(&self) -> u64 {
        self.queue_full
            .load(Ordering::Relaxed)
            .saturating_add(self.failed.load(Ordering::Relaxed))
    }

    #[cfg(test)]
    fn load(&self, disposition: ResolveEventDisposition) -> u64 {
        match disposition {
            ResolveEventDisposition::DroppedQueueFull => self.queue_full.load(Ordering::Relaxed),
            ResolveEventDisposition::DroppedByPolicy => self.policy.load(Ordering::Relaxed),
            ResolveEventDisposition::Accepted | ResolveEventDisposition::Disabled => 0,
        }
    }
}

impl DnsCore for ObservedDnsCore {
    fn resolve<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> crate::ports::PortFuture<'a, Result<CoreOutcome, CoreError>> {
        Box::pin(async move {
            let (result, observation) = self.inner.resolve_with_observation(request).await;
            self.record(request, &result, observation.as_ref());
            result
        })
    }
}

impl ObservedDnsCore {
    fn record(
        &self,
        request: &DnsRequest,
        result: &Result<CoreOutcome, CoreError>,
        observation: Option<&crate::dns::DnsResolutionObservation>,
    ) {
        let outcome = outcome_class(request, result);
        if let Some(worker) = &self.stats_worker {
            match day_utc(request.context.meta.received_at_utc) {
                Ok(day) => {
                    let mut dimensions = vec![
                        StatsDimension::transport(request.context.transport.class),
                        StatsDimension::attempt_outcome(outcome),
                    ];
                    if let Some(rcode) = response_rcode(result) {
                        dimensions.push(StatsDimension::rcode(rcode));
                    }
                    if let Some(observation) = observation {
                        dimensions.push(StatsDimension::source(observation.source));
                        dimensions.push(StatsDimension::cache_status(observation.cache_status));
                        if let Some(client_bucket) =
                            observation.client_bucket.as_deref().and_then(|id| {
                                configured_id_from_validated(ConfiguredIdKind::ClientBucket, id)
                            })
                            && let Ok(dimension) = StatsDimension::client_bucket(client_bucket)
                        {
                            dimensions.push(dimension);
                        }
                        if let Some(strategy_id) =
                            observation.strategy_id.as_deref().and_then(|id| {
                                configured_id_from_validated(ConfiguredIdKind::Strategy, id)
                            })
                            && let Ok(dimension) = StatsDimension::strategy(strategy_id)
                        {
                            dimensions.push(dimension);
                        }
                        if let Some(upstream_id) = observation
                            .upstream_member_id
                            .as_deref()
                            .or(observation.upstream_id.as_deref())
                            .and_then(|id| {
                                configured_id_from_validated(ConfiguredIdKind::Upstream, id)
                            })
                            && let Ok(dimension) = StatsDimension::upstream(upstream_id)
                        {
                            dimensions.push(dimension);
                        }
                    }
                    if let Err(_error) = worker.record_request(day, dimensions) {
                        tracing::warn!(
                            event = "stats_record_failed",
                            component = "storage",
                            class = "recording",
                            "stats_record_failed"
                        );
                    }
                }
                Err(_error) => {
                    tracing::warn!(
                        event = "stats_record_failed",
                        component = "storage",
                        class = "recording",
                        "stats_record_failed"
                    );
                }
            }
        }

        let Some(sink) = &self.resolve_event_sink else {
            return;
        };
        let question = request.query.question();
        let event = ResolveEvent {
            occurred_at: SystemTime::now(),
            duration_started_at: request.context.meta.received_at,
            request_digest: Arc::from(format!("{:032x}", request.context.meta.request_id.0)),
            listener_id: Arc::from(request.context.meta.listener_id.as_ref()),
            route_id: request
                .context
                .meta
                .route_id
                .as_ref()
                .map(|route| Arc::from(route.as_ref())),
            client_bucket: observation.and_then(|value| value.client_bucket.clone()),
            strategy_id: observation.and_then(|value| value.strategy_id.clone()),
            upstream_id: observation.and_then(|value| value.upstream_id.clone()),
            upstream_member_id: observation.and_then(|value| value.upstream_member_id.clone()),
            matched_rule_source: observation
                .and_then(|value| value.matched_rule.as_ref())
                .map(|matched| resolve_rule_source(matched.source)),
            matched_resource_id: observation
                .and_then(|value| value.matched_rule.as_ref())
                .map(|matched| Arc::clone(&matched.resource_id)),
            matched_rule_ordinal: observation
                .and_then(|value| value.matched_rule.as_ref())
                .and_then(|matched| matched.ordinal),
            resource_version: observation
                .and_then(|value| value.matched_rule.as_ref())
                .and_then(|matched| matched.resource_version),
            transport: request.context.transport.class,
            qname: Arc::from(question.name().to_ascii()),
            qtype: u16::from(question.query_type()),
            qclass: u16::from(question.query_class()),
            rcode: response_header_rcode(result),
            cancellation_reason: request.context.meta.cancellation.reason(),
            outcome,
            source: observation
                .map(|value| value.source)
                .unwrap_or(crate::ports::storage::StatsSource::Upstream),
            cache_status: observation
                .map(|value| value.cache_status)
                .unwrap_or(CacheStatus::Disabled),
            runtime_revision: request.context.runtime_revision,
        };
        match sink.try_record(event) {
            Ok(disposition) => {
                if let Some(update) = self.resolve_detail_drops.record(disposition)
                    && let Some(telemetry) = &self.telemetry
                {
                    publish_resolve_detail_health(telemetry, update);
                }
            }
            Err(error) => {
                tracing::warn!(
                    event = "resolve_detail_record_failed",
                    component = "storage",
                    class = error.class().as_str(),
                    operation = error.operation(),
                    "resolve_detail_record_failed"
                );
                if let Some(update) = self.resolve_detail_drops.record_failure()
                    && let Some(telemetry) = &self.telemetry
                {
                    publish_resolve_detail_health(telemetry, update);
                }
            }
        }
    }
}

/// 将 DNS policy 的规则来源映射为稳定的存储契约，避免存储层依赖 policy 内部类型。
fn resolve_rule_source(source: crate::dns::MatchedRuleSource) -> ResolveRuleSource {
    match source {
        crate::dns::MatchedRuleSource::ListenerHosts => ResolveRuleSource::ListenerHosts,
        crate::dns::MatchedRuleSource::StrategyHosts => ResolveRuleSource::StrategyHosts,
        crate::dns::MatchedRuleSource::RuleSet => ResolveRuleSource::RuleSet,
    }
}

/// 仅从实际 DNS 响应提取完整 RCODE；无响应或 Core 错误不伪造统计值。
fn response_rcode(result: &Result<CoreOutcome, CoreError>) -> Option<u16> {
    match result {
        Ok(CoreOutcome::Response(response)) => {
            Some(u16::from(response.as_message().metadata.response_code))
        }
        Ok(CoreOutcome::NoResponse) | Err(_) => None,
    }
}

/// `resolve_log.rcode` 保持基础 DNS header 的 4-bit 契约；无响应时由 failure 分类区分。
fn response_header_rcode(result: &Result<CoreOutcome, CoreError>) -> u8 {
    response_rcode(result)
        .map(|rcode| u8::try_from(rcode & 0x0f).expect("4-bit RCODE must fit u8"))
        .unwrap_or(0)
}

fn outcome_class(request: &DnsRequest, result: &Result<CoreOutcome, CoreError>) -> OutcomeClass {
    match result {
        Ok(CoreOutcome::Response(response)) => match response.class() {
            ResponseClass::Positive | ResponseClass::NoData | ResponseClass::NxDomain => {
                OutcomeClass::Success
            }
            ResponseClass::Refused => OutcomeClass::Rejected,
            ResponseClass::ServFail | ResponseClass::Truncated | ResponseClass::Other(_) => {
                OutcomeClass::Failure
            }
        },
        Ok(CoreOutcome::NoResponse) => {
            if request.context.meta.cancellation.is_cancelled() {
                if matches!(
                    request.context.meta.cancellation.reason(),
                    Some(CancelReason::DeadlineExceeded)
                ) {
                    OutcomeClass::Timeout
                } else {
                    OutcomeClass::Cancelled
                }
            } else if request.context.meta.deadline.is_expired(Instant::now()) {
                OutcomeClass::Timeout
            } else {
                OutcomeClass::Dropped
            }
        }
        Err(_) => OutcomeClass::Failure,
    }
}

async fn wait_for_termination_signal() -> Result<(), ServiceError> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|_| ServiceError::Signal)?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|_| ServiceError::Signal),
            result = terminate.recv() => result.ok_or(ServiceError::Signal),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|_| ServiceError::Signal)
    }
}

fn publish_component_health(
    telemetry: &TelemetryWriter,
    component: TelemetryComponent,
    state: ComponentHealthState,
    safe_reason: Option<&'static str>,
) {
    let now = Instant::now();
    if let Err(error) = HealthSink::update(
        telemetry,
        ComponentHealthEvent {
            component,
            state,
            first_seen: now,
            last_changed: now,
            last_success: (state == ComponentHealthState::Healthy).then_some(now),
            retry_count: 0,
            stale_age_micros: None,
            persistence_gap: false,
            safe_reason,
        },
    ) {
        tracing::debug!(
            event = "telemetry_health_publish_failed",
            component = ?component,
            class = error.class().as_str(),
            operation = error.operation(),
            "telemetry_health_publish_failed"
        );
    }
}

/// 将详情 writer 的限频状态变化发布为低基数 Storage health 事件。
fn publish_resolve_detail_health(telemetry: &TelemetryWriter, update: ResolveDetailHealthUpdate) {
    let now = Instant::now();
    if let Err(error) = HealthSink::update(
        telemetry,
        ComponentHealthEvent {
            component: TelemetryComponent::Storage,
            state: update.state,
            first_seen: now,
            last_changed: now,
            last_success: (update.state == ComponentHealthState::Healthy).then_some(now),
            retry_count: update.retry_count,
            stale_age_micros: None,
            persistence_gap: update.state != ComponentHealthState::Healthy,
            safe_reason: update.safe_reason,
        },
    ) {
        tracing::debug!(
            event = "telemetry_health_publish_failed",
            component = ?TelemetryComponent::Storage,
            class = error.class().as_str(),
            operation = error.operation(),
            "telemetry_health_publish_failed"
        );
    }
}

fn telemetry_component_for_task(component: &'static str) -> TelemetryComponent {
    match component {
        "storage" => TelemetryComponent::Storage,
        "resource" => TelemetryComponent::Resource,
        "udp" | "tcp" | "doh" => TelemetryComponent::Listener,
        "telemetry" => TelemetryComponent::Telemetry,
        _ => TelemetryComponent::Runtime,
    }
}

/// 发布 cache persistence 的停机健康状态，不把 key、响应或 adapter 错误写入 telemetry。
fn publish_cache_shutdown_health(
    telemetry: &TelemetryWriter,
    summary: CacheFinalizerShutdownSummary,
) {
    let now = Instant::now();
    let persistence_gap = !summary.completed || summary.persistence.has_persistence_gap();
    let event = ComponentHealthEvent {
        component: TelemetryComponent::Cache,
        state: if persistence_gap {
            ComponentHealthState::Degraded
        } else {
            ComponentHealthState::Stopping
        },
        first_seen: now,
        last_changed: now,
        last_success: None,
        retry_count: summary.persistence.failed_batches,
        stale_age_micros: None,
        persistence_gap,
        safe_reason: persistence_gap.then_some("cache persistence shutdown has gaps"),
    };
    if let Err(error) = HealthSink::update(telemetry, event) {
        tracing::debug!(
            event = "telemetry_health_publish_failed",
            component = ?TelemetryComponent::Cache,
            class = error.class().as_str(),
            operation = error.operation(),
            "telemetry_health_publish_failed"
        );
    }
}

/// 输出不含缓存 key 或响应内容的停机摘要，供核对 best-effort 持久化缺口。
fn log_cache_shutdown_summary(summary: CacheFinalizerShutdownSummary) {
    tracing::info!(
        event = "cache_shutdown_summary",
        component = "cache",
        owners = summary.owners,
        completed = summary.completed,
        persisted_batches = summary.persistence.persisted_batches,
        failed_batches = summary.persistence.failed_batches,
        dropped_batches = summary.persistence.dropped_batches,
        capacity_removed = summary.persistence.capacity_removed,
        persistence_gap = !summary.completed || summary.persistence.has_persistence_gap(),
        "cache_shutdown_summary"
    );
}

/// 输出不含请求内容的存储停机摘要，供正常停机后核对持久化缺口。
fn log_storage_shutdown_summary(summary: StorageServiceFlushSummary) {
    tracing::info!(
        event = "storage_shutdown_summary",
        component = "storage",
        stats_batches_committed = summary.stats.batches_committed,
        stats_events_committed = summary.stats.events_committed,
        stats_pending_batches = summary.stats.pending_batches,
        stats_persistence_gap = summary.stats.persistence_gap,
        backend_stats_committed = summary.storage.stats_committed,
        backend_details_committed = summary.storage.details_committed,
        backend_details_dropped = summary.storage.details_dropped,
        backend_persistence_gap = summary.storage.persistence_gap,
        resolve_log_committed = summary.resolve_log.flush.committed,
        resolve_log_pending = summary.resolve_log.flush.pending,
        resolve_log_dropped_queue_full = summary.resolve_log.flush.dropped_queue_full,
        resolve_log_sink_failures = summary.resolve_log.flush.sink_failures,
        resolve_log_discarded_pending = summary.resolve_log.discarded_pending,
        detail_committed = summary.detail.committed,
        detail_evicted = summary.detail.evicted,
        detail_dropped = summary.detail.dropped,
        "storage_shutdown_summary"
    );
}

async fn storage_flush_task(
    storage: Arc<tokio::sync::Mutex<StorageRuntime>>,
    cancellation: Cancellation,
    telemetry: Option<Arc<TelemetryWriter>>,
) -> Result<(), TaskError> {
    let mut interval = tokio::time::interval(DEFAULT_STORAGE_FLUSH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            _ = interval.tick() => {
                let deadline = Deadline::new(Instant::now() + DEFAULT_STORAGE_OPERATION_TIMEOUT);
                let mut storage = storage.lock().await;
                match storage.flush(deadline).await {
                    Ok(_) => {
                        if let Some(telemetry) = &telemetry {
                            publish_component_health(
                                telemetry,
                                TelemetryComponent::Storage,
                                ComponentHealthState::Healthy,
                                None,
                            );
                        }
                    }
                    Err(error) if error.is_fatal() => {
                        if let Some(telemetry) = &telemetry {
                            publish_component_health(
                                telemetry,
                                TelemetryComponent::Storage,
                                ComponentHealthState::Failed,
                                Some("storage flush reached fatal limit"),
                            );
                        }
                        tracing::error!(
                            event = "storage_pending_limit_exceeded",
                            component = "storage",
                            error = %error,
                            "storage_pending_limit_exceeded"
                        );
                        return Err(TaskError::Fatal);
                    }
                    Err(error) => {
                        if let Some(telemetry) = &telemetry {
                            publish_component_health(
                                telemetry,
                                TelemetryComponent::Storage,
                                ComponentHealthState::Degraded,
                                Some("storage flush failed"),
                            );
                        }
                        tracing::warn!(
                            event = "storage_flush_failed",
                            component = "storage",
                            error = %error,
                            "storage_flush_failed"
                        );
                    }
                }
            }
        }
    }
}

fn spawn_storage_task(
    supervisor: &mut Supervisor,
    storage: Arc<tokio::sync::Mutex<StorageRuntime>>,
    telemetry: Option<Arc<TelemetryWriter>>,
) -> Result<Cancellation, ServiceStartError> {
    let spec = TaskSpec::new(
        "storage.writer",
        "storage",
        FaultLevel::Fatal,
        RestartPolicy::Never,
    )
    .map_err(|error| ServiceStartError::Endpoint {
        index: 0,
        kind: "storage",
        reason: error.to_string(),
    })?;
    supervisor
        .spawn_scoped(spec, move |cancellation| {
            Box::pin(storage_flush_task(storage, cancellation, telemetry))
        })
        .map_err(ServiceStartError::Task)
}

async fn telemetry_flush_task(
    telemetry: Arc<TelemetryWriter>,
    cancellation: Cancellation,
) -> Result<(), TaskError> {
    let mut interval = tokio::time::interval(TELEMETRY_FLUSH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            _ = interval.tick() => {
                flush_telemetry_once(&telemetry).await;
            }
        }
    }
}

/// 执行一次有界 Telemetry flush，并把当前输出结果映射为 health 生命周期。
async fn flush_telemetry_once(telemetry: &TelemetryWriter) {
    let deadline = Deadline::new(Instant::now() + TELEMETRY_OPERATION_TIMEOUT);
    match LogSink::flush(telemetry, deadline).await {
        Ok(_) => publish_component_health(
            telemetry,
            TelemetryComponent::Telemetry,
            ComponentHealthState::Healthy,
            None,
        ),
        Err(error) => {
            publish_component_health(
                telemetry,
                TelemetryComponent::Telemetry,
                ComponentHealthState::Failed,
                Some("telemetry flush failed"),
            );
            tracing::warn!(
                event = "telemetry_flush_failed",
                component = "telemetry",
                class = error.class().as_str(),
                operation = error.operation(),
                "telemetry_flush_failed"
            );
        }
    }
}

fn spawn_telemetry_task(
    supervisor: &mut Supervisor,
    telemetry: Arc<TelemetryWriter>,
) -> Result<Cancellation, ServiceStartError> {
    let spec = TaskSpec::new(
        "telemetry.flush",
        "telemetry",
        FaultLevel::Degraded,
        RestartPolicy::Never,
    )
    .map_err(|error| ServiceStartError::Endpoint {
        index: 0,
        kind: "telemetry",
        reason: error.to_string(),
    })?;
    supervisor
        .spawn_scoped(spec, move |cancellation| {
            Box::pin(telemetry_flush_task(telemetry, cancellation))
        })
        .map_err(ServiceStartError::Task)
}

fn map_reload_spawn_error(error: ServiceStartError) -> ServiceReloadError {
    match error {
        ServiceStartError::Task(source) => ServiceReloadError::Task(source),
        other => ServiceReloadError::Endpoint(other),
    }
}

enum TransportTaskPlan {
    Udp {
        index: usize,
        owner: String,
        adapter: UdpAdapter,
    },
    Tcp {
        index: usize,
        owner: String,
        adapter: TcpAdapter,
    },
    Doh {
        index: usize,
        owner: String,
        adapter: DohAdapter,
    },
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
            let owner = entry.owner.clone();
            if entry.transport == BindTransport::Doh {
                let adapter = DohAdapter::from_endpoint(
                    BoundEndpointHandle { entry, socket },
                    config,
                    revision,
                    transport_capabilities(TransportClass::Multiplexed),
                    request_timeout,
                )
                .map_err(|reason| ServiceStartError::Endpoint {
                    index,
                    kind: "DoH",
                    reason: reason.to_string(),
                })?;
                return Ok(TransportTaskPlan::Doh {
                    index,
                    owner,
                    adapter,
                });
            }
            match socket {
                ActivatedSocketHandle::Udp(socket) => {
                    let adapter = UdpAdapter::from_endpoint(
                        BoundEndpointHandle {
                            entry,
                            socket: ActivatedSocketHandle::Udp(socket),
                        },
                        revision,
                        transport_capabilities(TransportClass::Datagram),
                        request_timeout,
                    )
                    .map_err(|reason| ServiceStartError::Endpoint {
                        index,
                        kind: "UDP",
                        reason: reason.to_string(),
                    })?;
                    Ok(TransportTaskPlan::Udp {
                        index,
                        owner,
                        adapter,
                    })
                }
                ActivatedSocketHandle::Tcp(listener) => {
                    let adapter = TcpAdapter::from_endpoint(
                        BoundEndpointHandle {
                            entry,
                            socket: ActivatedSocketHandle::Tcp(listener),
                        },
                        revision,
                        transport_capabilities(TransportClass::Stream),
                        request_timeout,
                    )
                    .map_err(|reason| ServiceStartError::Endpoint {
                        index,
                        kind: "TCP",
                        reason: reason.to_string(),
                    })?;
                    Ok(TransportTaskPlan::Tcp {
                        index,
                        owner,
                        adapter,
                    })
                }
            }
        })
        .collect()
}

/// 注册 transport task，并保留 current-revision 故障聚合所需的身份和 owner。
fn spawn_transport_plans(
    supervisor: &mut Supervisor,
    plans: Vec<TransportTaskPlan>,
    core: Arc<dyn DnsCore>,
    runtime: Arc<ActiveRuntime>,
) -> Result<Vec<TransportTask>, ServiceStartError> {
    let revision = runtime.revision().0;
    let mut tasks = Vec::with_capacity(plans.len());
    for plan in plans {
        let (task_id, owner, cancellation) = match plan {
            TransportTaskPlan::Udp {
                index,
                owner,
                adapter,
            } => {
                let task_core = Arc::clone(&core);
                let task_runtime = Arc::clone(&runtime);
                let task_id = format!("transport.udp.{revision}.{index}");
                let cancellation = spawn_transport_task(
                    supervisor,
                    task_id.clone(),
                    "udp",
                    move |cancellation| {
                        service_task(
                            adapter.clone(),
                            Arc::clone(&task_core),
                            Arc::clone(&task_runtime),
                            cancellation,
                        )
                    },
                )?;
                (task_id, owner, cancellation)
            }
            TransportTaskPlan::Tcp {
                index,
                owner,
                adapter,
            } => {
                let task_core = Arc::clone(&core);
                let task_runtime = Arc::clone(&runtime);
                let task_id = format!("transport.tcp.{revision}.{index}");
                let cancellation = spawn_transport_task(
                    supervisor,
                    task_id.clone(),
                    "tcp",
                    move |cancellation| {
                        tcp_listener_task(
                            adapter.clone(),
                            Arc::clone(&task_core),
                            Arc::clone(&task_runtime),
                            cancellation,
                        )
                    },
                )?;
                (task_id, owner, cancellation)
            }
            TransportTaskPlan::Doh {
                index,
                owner,
                adapter,
            } => {
                let task_core = Arc::clone(&core);
                let task_runtime = Arc::clone(&runtime);
                let task_id = format!("transport.doh.{revision}.{index}");
                let cancellation = spawn_transport_task(
                    supervisor,
                    task_id.clone(),
                    "doh",
                    move |cancellation| {
                        doh_listener_task(
                            adapter.clone(),
                            Arc::clone(&task_core),
                            Arc::clone(&task_runtime),
                            cancellation,
                        )
                    },
                )?;
                (task_id, owner, cancellation)
            }
        };
        tasks.push(TransportTask {
            task_id,
            owner,
            cancellation,
        });
    }
    Ok(tasks)
}

/// 判断 transport endpoint 是否已耗尽瞬时重试，且应进入 endpoint 聚合判定。
fn is_exhausted_endpoint(completion: &TaskCompletion) -> bool {
    completion.spec.fault_level == FaultLevel::FatalEndpoint && completion.restart_exhausted()
}

/// 从当前 Runtime 移除已失效 endpoint，返回同一逻辑 listener 的剩余 endpoint 数量。
///
/// 返回 `None` 表示完成事件属于旧 Runtime；reload 后的迟到事件不能影响新 Runtime。
fn retire_current_transport_task(
    tasks: &mut Vec<TransportTask>,
    completion: &TaskCompletion,
) -> Option<usize> {
    let index = tasks
        .iter()
        .position(|task| task.task_id == completion.spec.id.as_str())?;
    let retired = tasks.swap_remove(index);
    Some(
        tasks
            .iter()
            .filter(|task| task.owner == retired.owner)
            .count(),
    )
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
    telemetry: Option<Arc<TelemetryWriter>>,
) -> Result<Vec<ResourceTask>, ServiceStartError> {
    let mut cancellations = Vec::with_capacity(resources.len());
    for (index, resource) in resources.into_iter().enumerate() {
        cancellations.push(spawn_resource_task(
            supervisor,
            Arc::clone(&coordinator),
            revision,
            index,
            resource,
            telemetry.clone(),
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
    telemetry: Option<Arc<TelemetryWriter>>,
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
            resource_refresh_task(task_coordinator, task_resource, cancellation, telemetry)
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
    telemetry: Option<Arc<TelemetryWriter>>,
) -> crate::runtime::TaskFuture {
    Box::pin(async move {
        run_resource_refresh_loop(coordinator, resource, cancellation, telemetry).await
    })
}

async fn run_resource_refresh_loop(
    coordinator: Arc<RuntimeCoordinator>,
    resource: ConfigId,
    cancellation: Cancellation,
    telemetry: Option<Arc<TelemetryWriter>>,
) -> Result<(), TaskError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(TaskError::Cancelled);
        }
        let now = unix_seconds();
        let runtime = coordinator.load();
        let Some(decision) = runtime.resource_refresh_decision(&resource, now) else {
            if !cancellation.is_cancelled()
                && let Some(telemetry) = &telemetry
            {
                publish_component_health(
                    telemetry,
                    TelemetryComponent::Resource,
                    ComponentHealthState::Failed,
                    Some("resource worker is not configured"),
                );
            }
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
                Ok(snapshot) => {
                    if let Some(telemetry) = &telemetry {
                        publish_component_health(
                            telemetry,
                            TelemetryComponent::Resource,
                            ComponentHealthState::Healthy,
                            None,
                        );
                    }
                    tracing::info!(
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
                    )
                }
                Err(ResourceRefreshCoordinatorError::Stale { .. }) => continue,
                Err(_error) if cancellation.is_cancelled() => {
                    runtime.shutdown_resource_refresh();
                    return Err(TaskError::Cancelled);
                }
                Err(error) => {
                    if let Some(telemetry) = &telemetry {
                        publish_component_health(
                            telemetry,
                            TelemetryComponent::Resource,
                            ComponentHealthState::Degraded,
                            Some("resource refresh failed"),
                        );
                    }
                    tracing::warn!(
                        event = "resource_refresh_failed",
                        component = "resource",
                        resource = %resource.as_str(),
                        error = %error,
                        "resource_refresh_failed"
                    )
                }
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
            accepted = adapter.accept_session(&cancellation),
                if !cancellation.is_cancelled() && can_accept_stream_session(sessions.len()) => {
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
                    Err(error) if is_listener_idle_timeout(&error) => continue,
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
            accepted = adapter.accept_session(&cancellation),
                if !cancellation.is_cancelled() && can_accept_stream_session(sessions.len()) => {
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
                    Err(error) if is_listener_idle_timeout(&error) => continue,
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

/// 判断 TCP/DoH listener 是否仍可接收新 session，避免慢连接无限扩张 task 集合。
fn can_accept_stream_session(active_sessions: usize) -> bool {
    active_sessions < MAX_CONCURRENT_STREAM_SESSIONS
}

/// 识别 listener 在无流量期间的正常 deadline 轮询，不把空闲误计为 endpoint 故障。
fn is_listener_idle_timeout(error: &crate::ports::PortError) -> bool {
    matches!(error.class(), PortErrorClass::Timeout)
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
                if is_listener_idle_timeout(&error) {
                    continue;
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
    use std::net::{
        Ipv4Addr, SocketAddr, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, RecordType};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UdpSocket};

    use super::{
        MAX_CONCURRENT_STREAM_SESSIONS, ResolveDetailDropCounters, ResolveDetailHealthUpdate,
        ServiceError, TransportTask, can_accept_stream_session, flush_telemetry_once,
        is_exhausted_endpoint, publish_cache_shutdown_health, publish_component_health,
        publish_resolve_detail_health, response_header_rcode, response_rcode,
        retire_current_transport_task, spawn_telemetry_task, spawn_transport_task, task_failure,
    };
    use crate::cache::CachePersistenceRunSummary;
    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::{
        CacheCompatibilityKey, CancelReason, Cancellation, CanonicalQuery, CanonicalResponse,
        CoreError, CoreOutcome, Deadline, DnsCore, DnsMessageId, DnsRequest, ResponseClass,
        RuntimeRevision, TransportClass,
    };
    use crate::observability::{TelemetryOutput, TelemetryWriter};
    use crate::ports::storage::ResolveEventDisposition;
    use crate::ports::telemetry::{
        Component as TelemetryComponent, ComponentHealthEvent, ComponentHealthState, LogEvent,
        LogLevel, MetricEvent,
    };
    use crate::runtime::{
        CacheFinalizerShutdownSummary, FaultLevel, PreparedRuntime, RestartPolicy,
        RuntimeCoordinator, Supervisor, SystemClock, SystemSocketFactory, TaskCompletion,
        TaskError, TaskErrorKind, TaskExit, TaskSpec,
    };
    use crate::storage::StorageRuntime;
    use crate::transport::transport_capabilities;

    #[derive(Default)]
    struct CountingTelemetryOutput {
        fail: AtomicBool,
        logs: AtomicUsize,
        metrics: AtomicUsize,
        health: AtomicUsize,
        health_events: Mutex<Vec<ComponentHealthEvent>>,
    }

    /// 为真实跨 transport 测试生成稳定的 SERVFAIL/REFUSED 响应。
    struct CrossTransportErrorCore;

    impl DnsCore for CrossTransportErrorCore {
        /// A 查询返回 SERVFAIL，AAAA 查询返回 REFUSED，避免测试依赖外部上游。
        fn resolve<'a>(
            &'a self,
            request: &'a DnsRequest,
        ) -> crate::ports::PortFuture<'a, Result<CoreOutcome, CoreError>> {
            Box::pin(async move {
                let code = match request.query.question().query_type() {
                    RecordType::A => ResponseCode::ServFail,
                    RecordType::AAAA => ResponseCode::Refused,
                    record_type => panic!("unexpected error-contract record type: {record_type}"),
                };
                CanonicalResponse::empty_response(&request.query, code)
                    .map(CoreOutcome::Response)
                    .map_err(CoreError::ResponseConstruction)
            })
        }
    }

    #[test]
    fn response_rcode_only_reports_actual_dns_responses() {
        let mut message = Message::new(9, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_ascii("rcode.example.").unwrap(),
            RecordType::A,
        ));
        let query = CanonicalQuery::from_message(message).unwrap();
        let response = CanonicalResponse::empty_response(&query, ResponseCode::Refused).unwrap();

        assert_eq!(
            response_rcode(&Ok(CoreOutcome::Response(response))),
            Some(5)
        );
        let response = CanonicalResponse::empty_response(&query, ResponseCode::Refused).unwrap();
        assert_eq!(
            response_header_rcode(&Ok(CoreOutcome::Response(response))),
            5
        );
        assert_eq!(response_rcode(&Ok(CoreOutcome::NoResponse)), None);
        assert_eq!(response_header_rcode(&Ok(CoreOutcome::NoResponse)), 0);
    }

    #[test]
    fn resolve_detail_drops_are_counted_by_reason() {
        let counters = ResolveDetailDropCounters::default();

        assert_eq!(counters.record(ResolveEventDisposition::Accepted), None);
        assert_eq!(counters.record(ResolveEventDisposition::Disabled), None);
        let first = counters
            .record(ResolveEventDisposition::DroppedQueueFull)
            .unwrap();
        assert_eq!(first.state, ComponentHealthState::Degraded);
        assert_eq!(first.retry_count, 1);
        assert_eq!(first.safe_reason, Some("resolve detail queue is full"));
        assert_eq!(
            counters
                .record(ResolveEventDisposition::DroppedQueueFull)
                .unwrap()
                .retry_count,
            2
        );
        assert_eq!(
            counters.record(ResolveEventDisposition::DroppedByPolicy),
            None
        );
        let failed = counters.record_failure().unwrap();
        assert_eq!(failed.retry_count, 3);
        assert_eq!(failed.safe_reason, Some("resolve detail record failed"));
        let recovered = counters.record(ResolveEventDisposition::Accepted).unwrap();
        assert_eq!(recovered.state, ComponentHealthState::Healthy);
        assert_eq!(recovered.retry_count, 3);
        assert_eq!(recovered.safe_reason, None);
        assert_eq!(counters.record(ResolveEventDisposition::Accepted), None);

        assert_eq!(counters.load(ResolveEventDisposition::DroppedQueueFull), 2);
        assert_eq!(counters.load(ResolveEventDisposition::DroppedByPolicy), 1);
        assert_eq!(counters.load(ResolveEventDisposition::Accepted), 0);
    }

    impl TelemetryOutput for CountingTelemetryOutput {
        fn write_log(&self, _event: &LogEvent) -> Result<(), crate::ports::PortError> {
            self.check("test.telemetry.log")?;
            self.logs.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn write_metric(&self, _event: &MetricEvent) -> Result<(), crate::ports::PortError> {
            self.check("test.telemetry.metric")?;
            self.metrics.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn write_health(
            &self,
            event: &ComponentHealthEvent,
        ) -> Result<(), crate::ports::PortError> {
            self.check("test.telemetry.health")?;
            self.health.fetch_add(1, Ordering::Relaxed);
            self.health_events.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    impl CountingTelemetryOutput {
        /// 为 Service 测试提供可恢复的安全输出故障注入。
        fn check(&self, operation: &'static str) -> Result<(), crate::ports::PortError> {
            if self.fail.load(Ordering::Acquire) {
                Err(crate::ports::PortError::new(
                    crate::ports::PortErrorClass::Unavailable,
                    operation,
                ))
            } else {
                Ok(())
            }
        }
    }

    fn telemetry_log() -> LogEvent {
        LogEvent {
            occurred_at: SystemTime::now(),
            level: LogLevel::Info,
            name: crate::ports::telemetry::EventName::parse("service.test").unwrap(),
            component: TelemetryComponent::Application,
            request_digest: None,
            configured_id: None,
            outcome: crate::ports::telemetry::OutcomeClass::Success,
            runtime_revision: None,
            message: "service test",
        }
    }

    #[tokio::test]
    async fn telemetry_flush_task_drains_writer_under_supervisor() {
        let output = Arc::new(CountingTelemetryOutput::default());
        let writer = Arc::new(TelemetryWriter::new(4, output.clone()).unwrap());
        crate::ports::telemetry::LogSink::emit(writer.as_ref(), telemetry_log()).unwrap();

        let mut supervisor = Supervisor::new();
        let cancellation = spawn_telemetry_task(&mut supervisor, writer).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(output.logs.load(Ordering::Relaxed), 1);

        cancellation.cancel(CancelReason::Shutdown);
        let report = supervisor
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(1)),
            )
            .await;
        assert_eq!(report.failed, 0);
    }

    /// 验证输出故障进入 Failed，恢复后不会被历史累计失败数永久误判。
    #[tokio::test]
    async fn telemetry_flush_health_recovers_after_output_returns() {
        let output = Arc::new(CountingTelemetryOutput::default());
        let writer = TelemetryWriter::new(4, output.clone()).unwrap();
        crate::ports::telemetry::LogSink::emit(&writer, telemetry_log()).unwrap();
        output.fail.store(true, Ordering::Release);

        flush_telemetry_once(&writer).await;
        assert_eq!(writer.stats().failed(), 1);
        output.fail.store(false, Ordering::Release);
        flush_telemetry_once(&writer).await;
        crate::ports::telemetry::LogSink::flush(
            &writer,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap();

        let states = output
            .health_events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.component == TelemetryComponent::Telemetry)
            .map(|event| event.state)
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![ComponentHealthState::Failed, ComponentHealthState::Healthy]
        );
    }

    #[tokio::test]
    async fn component_health_is_published_as_a_bounded_telemetry_event() {
        let output = Arc::new(CountingTelemetryOutput::default());
        let writer = TelemetryWriter::new(4, output.clone()).unwrap();
        publish_component_health(
            &writer,
            TelemetryComponent::Storage,
            ComponentHealthState::Degraded,
            Some("storage flush failed"),
        );
        publish_component_health(
            &writer,
            TelemetryComponent::Resource,
            ComponentHealthState::Healthy,
            None,
        );

        crate::ports::telemetry::LogSink::flush(
            &writer,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert_eq!(output.health.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cache_shutdown_gap_is_published_before_telemetry_closes() {
        let output = Arc::new(CountingTelemetryOutput::default());
        let writer = TelemetryWriter::new(4, output.clone()).unwrap();
        publish_cache_shutdown_health(
            &writer,
            CacheFinalizerShutdownSummary {
                completed: true,
                owners: 2,
                persistence: CachePersistenceRunSummary {
                    persisted_batches: 7,
                    failed_batches: 3,
                    dropped_batches: 1,
                    capacity_removed: 4,
                },
            },
        );

        crate::ports::telemetry::LogSink::flush(
            &writer,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap();
        let health_events = output.health_events.lock().unwrap();
        assert_eq!(health_events.len(), 1);
        assert_eq!(health_events[0].component, TelemetryComponent::Cache);
        assert_eq!(health_events[0].state, ComponentHealthState::Degraded);
        assert_eq!(health_events[0].retry_count, 3);
        assert!(health_events[0].persistence_gap);
        assert_eq!(
            health_events[0].safe_reason,
            Some("cache persistence shutdown has gaps")
        );
    }

    #[tokio::test]
    async fn resolve_detail_health_publishes_gap_and_recovery() {
        let output = Arc::new(CountingTelemetryOutput::default());
        let writer = TelemetryWriter::new(4, output.clone()).unwrap();
        publish_resolve_detail_health(
            &writer,
            ResolveDetailHealthUpdate {
                state: ComponentHealthState::Degraded,
                retry_count: 2,
                safe_reason: Some("resolve detail queue is full"),
            },
        );
        publish_resolve_detail_health(
            &writer,
            ResolveDetailHealthUpdate {
                state: ComponentHealthState::Healthy,
                retry_count: 2,
                safe_reason: None,
            },
        );

        crate::ports::telemetry::LogSink::flush(
            &writer,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap();
        let health_events = output.health_events.lock().unwrap();
        assert_eq!(health_events.len(), 2);
        assert_eq!(health_events[0].component, TelemetryComponent::Storage);
        assert_eq!(health_events[0].state, ComponentHealthState::Degraded);
        assert!(health_events[0].persistence_gap);
        assert_eq!(health_events[1].state, ComponentHealthState::Healthy);
        assert_eq!(health_events[1].retry_count, 2);
        assert!(!health_events[1].persistence_gap);
        assert!(health_events[1].last_success.is_some());
    }

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
            transport_capabilities(TransportClass::Datagram),
            crate::dns::TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            }
        );
        assert_eq!(
            transport_capabilities(TransportClass::Stream),
            crate::dns::TransportCapabilities {
                class: TransportClass::Stream,
                cache_compatibility: CacheCompatibilityKey(1),
            }
        );
        assert_eq!(
            transport_capabilities(TransportClass::Multiplexed),
            crate::dns::TransportCapabilities {
                class: TransportClass::Multiplexed,
                cache_compatibility: CacheCompatibilityKey(1),
            }
        );
    }

    /// 验证 TCP/DoH session 并发上限在边界前允许接收，达到边界后暂停 accept。
    #[test]
    fn stream_session_limit_is_enforced_at_accept_boundary() {
        assert!(can_accept_stream_session(
            MAX_CONCURRENT_STREAM_SESSIONS - 1
        ));
        assert!(!can_accept_stream_session(MAX_CONCURRENT_STREAM_SESSIONS));
        assert!(!can_accept_stream_session(usize::MAX));
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
    fn storage_pending_limit_task_failure_is_promoted_to_service_error() {
        let completion = completion(
            FaultLevel::Fatal,
            RestartPolicy::Never,
            TaskExit::Failed(TaskErrorKind::Fatal),
            0,
        );

        assert!(matches!(
            task_failure(&completion),
            Some(ServiceError::TaskFailure {
                fault_level: FaultLevel::Fatal,
                exit: TaskExit::Failed(TaskErrorKind::Fatal),
                ..
            })
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

    #[tokio::test]
    async fn transport_task_exhaustion_is_promoted_after_configured_retries() {
        let mut supervisor = Supervisor::new();
        let attempts = Arc::new(AtomicU32::new(0));
        let factory_attempts = Arc::clone(&attempts);
        spawn_transport_task(
            &mut supervisor,
            "transport.exhausted".to_owned(),
            "udp",
            move |_cancellation| {
                factory_attempts.fetch_add(1, Ordering::AcqRel);
                Box::pin(async { Err(TaskError::Transient) })
            },
        )
        .unwrap();

        let completion = tokio::time::timeout(Duration::from_secs(1), supervisor.join_next())
            .await
            .unwrap()
            .unwrap();

        // max_restarts 不包含初次执行，因此总尝试次数应再加一。
        assert_eq!(
            attempts.load(Ordering::Acquire),
            super::TRANSPORT_RESTART_LIMIT + 1
        );
        assert_eq!(completion.restart_count, super::TRANSPORT_RESTART_LIMIT);
        assert!(completion.restart_exhausted());
        assert!(matches!(
            task_failure(&completion),
            Some(ServiceError::TaskFailure {
                fault_level: FaultLevel::FatalEndpoint,
                exit: TaskExit::Failed(TaskErrorKind::Transient),
                ..
            })
        ));
        assert_eq!(supervisor.task_count(), 0);
    }

    #[test]
    fn exhausted_endpoint_only_stops_service_after_listener_group_is_empty() {
        let mut tasks = vec![
            TransportTask {
                task_id: "transport.udp.2.0".to_owned(),
                owner: "listener.primary".to_owned(),
                cancellation: Cancellation::new(),
            },
            TransportTask {
                task_id: "transport.tcp.2.1".to_owned(),
                owner: "listener.primary".to_owned(),
                cancellation: Cancellation::new(),
            },
            TransportTask {
                task_id: "transport.udp.2.2".to_owned(),
                owner: "listener.secondary".to_owned(),
                cancellation: Cancellation::new(),
            },
        ];
        let first = completion(
            FaultLevel::FatalEndpoint,
            RestartPolicy::Transient { max_restarts: 3 },
            TaskExit::Failed(TaskErrorKind::Transient),
            3,
        );
        let first = TaskCompletion {
            spec: TaskSpec::new(
                "transport.udp.2.0",
                "udp",
                first.spec.fault_level,
                first.spec.restart_policy,
            )
            .unwrap(),
            ..first
        };

        assert!(is_exhausted_endpoint(&first));
        assert_eq!(retire_current_transport_task(&mut tasks, &first), Some(1));

        let stale = TaskCompletion {
            spec: TaskSpec::new(
                "transport.udp.1.0",
                "udp",
                FaultLevel::FatalEndpoint,
                RestartPolicy::Transient { max_restarts: 3 },
            )
            .unwrap(),
            exit: TaskExit::Failed(TaskErrorKind::Transient),
            restart_count: 3,
        };
        assert_eq!(retire_current_transport_task(&mut tasks, &stale), None);

        let last = TaskCompletion {
            spec: TaskSpec::new(
                "transport.tcp.2.1",
                "tcp",
                FaultLevel::FatalEndpoint,
                RestartPolicy::Transient { max_restarts: 3 },
            )
            .unwrap(),
            exit: TaskExit::Failed(TaskErrorKind::Transient),
            restart_count: 3,
        };
        assert_eq!(retire_current_transport_task(&mut tasks, &last), Some(0));
        assert_eq!(tasks.len(), 1);
    }

    #[tokio::test]
    async fn fatal_task_flushes_process_services_before_returning_error() {
        let base_port = 42_000 + (std::process::id() as u16 % 500) * 2;
        let work_path = crate::config::test_support::absolute_path("service-fatal-task-shutdown");
        let initial_config = runtime_config_at(&work_path, base_port);
        let database_path = initial_config.database.path.clone();
        let factory = SystemSocketFactory::new();
        let initial = PreparedRuntime::prepare_with_policy_core(
            Arc::clone(&initial_config),
            crate::dns::RuntimeRevision(1),
        )
        .unwrap();
        let initial = crate::runtime::bind_prepared(
            initial,
            &factory,
            crate::dns::Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(initial));
        let previous = coordinator.load();
        let storage = StorageRuntime::open(
            &initial_config,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .unwrap();
        let output = Arc::new(CountingTelemetryOutput::default());
        let telemetry = Arc::new(TelemetryWriter::new(16, output).unwrap());
        let telemetry_probe = Arc::clone(&telemetry);
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator_storage_and_telemetry(
                Arc::clone(&coordinator),
                storage,
                telemetry,
            )
            .unwrap();

        let response = udp_query(
            SocketAddr::from((Ipv4Addr::LOCALHOST, base_port)),
            1,
            "example.test.",
        )
        .await;
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);

        let next = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, base_port + 1),
            crate::dns::RuntimeRevision(2),
        )
        .unwrap();
        let next = crate::runtime::bind_prepared(
            next,
            &factory,
            crate::dns::Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        coordinator.activate(next);
        let current = coordinator.load();

        service
            .supervisor
            .spawn(
                TaskSpec::new(
                    "fatal.test",
                    "test",
                    FaultLevel::Fatal,
                    RestartPolicy::Never,
                )
                .unwrap(),
                Box::pin(async { Err(TaskError::Fatal) }),
            )
            .unwrap();
        let error = service
            .wait_for_ctrl_c_with_reload(
                Duration::from_millis(100),
                Duration::from_secs(1),
                |_service| Box::pin(async { Ok(()) }),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ServiceError::TaskFailure { .. }));
        assert!(previous.is_draining());
        assert!(current.is_draining());
        assert!(telemetry_probe.stats().closed());
        assert_eq!(sqlite_total_requests(&database_path).await, 1);
        let _ = std::fs::remove_dir_all(work_path);
    }

    async fn sqlite_total_requests(path: &std::path::Path) -> i64 {
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let total =
            sqlx::query_scalar("SELECT COALESCE(SUM(total_requests), 0) FROM stats_daily_total")
                .fetch_one(&pool)
                .await
                .unwrap();
        pool.close().await;
        total
    }

    fn runtime_config_at(
        work_path: &str,
        port: u16,
    ) -> Arc<crate::config::resolve::ResolvedConfig> {
        runtime_config_with_answer_at(work_path, port, "127.0.0.1")
    }

    /// 构造 bind plan 不变、hosts answer 可变的 reload 测试配置。
    fn runtime_config_with_answer_at(
        work_path: &str,
        port: u16,
        answer: &str,
    ) -> Arc<crate::config::resolve::ResolvedConfig> {
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
    hosts: "{answer} example.test"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: "{answer} example.test"
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
                answer = answer,
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

    async fn udp_query(address: SocketAddr, id: u16, name: &str) -> Message {
        udp_query_with_type(address, id, name, RecordType::A).await
    }

    /// 通过 UDP 发送指定记录类型的真实 DNS 查询。
    async fn udp_query_with_type(
        address: SocketAddr,
        id: u16,
        name: &str,
        record_type: RecordType,
    ) -> Message {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let query = query_wire_with_type(id, name, record_type);
        socket.send_to(&query, address).await.unwrap();
        let mut response = [0_u8; 4096];
        let (size, _) =
            tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut response))
                .await
                .unwrap()
                .unwrap();
        Message::from_vec(&response[..size]).unwrap()
    }

    /// 为指定记录类型生成 DNS query wire。
    fn query_wire_with_type(id: u16, name: &str, record_type: RecordType) -> Vec<u8> {
        let mut query = Message::new(id, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(Name::from_ascii(name).unwrap(), record_type));
        query.to_vec().unwrap()
    }

    /// 通过 DNS-over-TCP framing 发送指定记录类型的查询。
    async fn tcp_query_with_type(
        address: SocketAddr,
        id: u16,
        name: &str,
        record_type: RecordType,
    ) -> Message {
        let query = query_wire_with_type(id, name, record_type);
        let mut request = Vec::with_capacity(query.len() + 2);
        request.extend_from_slice(&(query.len() as u16).to_be_bytes());
        request.extend_from_slice(&query);
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(&request).await.unwrap();

        let mut length = [0_u8; 2];
        tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut length))
            .await
            .unwrap()
            .unwrap();
        let mut response = vec![0_u8; u16::from_be_bytes(length) as usize];
        tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut response))
            .await
            .unwrap()
            .unwrap();
        Message::from_vec(&response).unwrap()
    }

    /// 通过 plain HTTP/1.1 POST 发送指定记录类型的 DoH 查询。
    async fn doh_post_query_with_type(
        address: SocketAddr,
        id: u16,
        name: &str,
        record_type: RecordType,
    ) -> Message {
        let query = query_wire_with_type(id, name, record_type);
        let mut request = format!(
            "POST /dns HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            query.len()
        )
        .into_bytes();
        request.extend_from_slice(&query);
        send_doh_request(address, &request).await
    }

    /// 通过 plain HTTP/1.1 GET 发送 unpadded base64url DoH 查询。
    async fn doh_get_query_with_type(
        address: SocketAddr,
        id: u16,
        name: &str,
        record_type: RecordType,
    ) -> Message {
        let query = query_wire_with_type(id, name, record_type);
        let request = format!(
            "GET /dns?dns={} HTTP/1.1\r\nHost: localhost\r\nAccept: application/dns-message\r\nConnection: close\r\n\r\n",
            base64url(&query)
        );
        send_doh_request(address, request.as_bytes()).await
    }

    /// 发送一条 plain DoH HTTP 请求并提取 DNS message body。
    async fn send_doh_request(address: SocketAddr, request: &[u8]) -> Message {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("DoH response must contain a complete HTTP header");
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        Message::from_vec(&response[header_end + 4..]).unwrap()
    }

    /// 将 DNS wire 编码为 DoH GET 使用的 unpadded base64url。
    fn base64url(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut output = String::new();
        let mut index = 0;
        while index < bytes.len() {
            let first = bytes[index];
            let second = bytes.get(index + 1).copied();
            let third = bytes.get(index + 2).copied();
            output.push(TABLE[(first >> 2) as usize] as char);
            output.push(TABLE[((first & 0x03) << 4 | second.unwrap_or(0) >> 4) as usize] as char);
            if let Some(second) = second {
                output
                    .push(TABLE[((second & 0x0f) << 2 | third.unwrap_or(0) >> 6) as usize] as char);
            }
            if let Some(third) = third {
                output.push(TABLE[(third & 0x3f) as usize] as char);
            }
            index += 3;
        }
        output
    }

    /// 让操作系统选择当前可绑定的 UDP、TCP 和 DoH loopback 端口。
    fn available_transport_ports() -> [u16; 3] {
        let udp = StdUdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let tcp = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let doh = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        [
            udp.local_addr().unwrap().port(),
            tcp.local_addr().unwrap().port(),
            doh.local_addr().unwrap().port(),
        ]
    }

    /// 对同一问题执行 UDP、TCP、DoH POST 和 DoH GET，并使用不同 ID 验证关联恢复。
    async fn query_all_transports(
        ports: [u16; 3],
        first_id: u16,
        name: &str,
        record_type: RecordType,
    ) -> [Message; 4] {
        let udp = udp_query_with_type(
            SocketAddr::from((Ipv4Addr::LOCALHOST, ports[0])),
            first_id,
            name,
            record_type,
        )
        .await;
        let tcp = tcp_query_with_type(
            SocketAddr::from((Ipv4Addr::LOCALHOST, ports[1])),
            first_id + 1,
            name,
            record_type,
        )
        .await;
        let doh_post = doh_post_query_with_type(
            SocketAddr::from((Ipv4Addr::LOCALHOST, ports[2])),
            first_id + 2,
            name,
            record_type,
        )
        .await;
        let doh_get = doh_get_query_with_type(
            SocketAddr::from((Ipv4Addr::LOCALHOST, ports[2])),
            first_id + 3,
            name,
            record_type,
        )
        .await;
        [udp, tcp, doh_post, doh_get]
    }

    /// 去除 transport 关联 ID 后，校验三种协议及两种 DoH method 返回同一响应。
    fn assert_cross_transport_contract(
        responses: [Message; 4],
        first_id: u16,
        name: &str,
        record_type: RecordType,
        expected_class: ResponseClass,
    ) -> CanonicalResponse {
        let canonical = responses
            .into_iter()
            .enumerate()
            .map(|(index, response)| {
                canonicalize_response(response, first_id + index as u16, name, record_type)
            })
            .collect::<Vec<_>>();
        assert_eq!(canonical[0], canonical[1]);
        assert_eq!(canonical[0], canonical[2]);
        assert_eq!(canonical[0], canonical[3]);
        assert_eq!(canonical[0].class(), expected_class);
        canonical[0].clone()
    }

    /// 校验响应 ID 与问题后，转换为不含 transport 关联信息的 canonical response。
    fn canonicalize_response(
        response: Message,
        id: u16,
        name: &str,
        record_type: RecordType,
    ) -> CanonicalResponse {
        assert_eq!(response.metadata.id, id);
        let query = Message::from_vec(&query_wire_with_type(id, name, record_type)).unwrap();
        let query = CanonicalQuery::from_message(query).unwrap();
        CanonicalResponse::from_message(response, &query, DnsMessageId::new(id)).unwrap()
    }

    /// 构造同时启用 UDP、TCP 和 plain DoH 的同策略 loopback 配置。
    fn cross_transport_runtime_config(
        udp_port: u16,
        tcp_port: u16,
        doh_port: u16,
    ) -> Arc<crate::config::resolve::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("service-cross-transport");
        let large_hosts = (1..=64)
            .map(|suffix| format!("198.51.100.{suffix} large.transport.test"))
            .collect::<Vec<_>>()
            .join("\n      ");
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
  - name: dns-udp
    type: udp
    addresses: [127.0.0.1]
    port: {udp_port}
    strategy: default
  - name: dns-tcp
    type: tcp
    addresses: [127.0.0.1]
    port: {tcp_port}
    strategy: default
  - name: dns-doh
    type: doh
    routes:
      - path: /dns
        strategy: default
    endpoints:
      - name: plain
        addresses: [127.0.0.1]
        port: {doh_port}
        tls:
          mode: external
        client_ip:
          source: peer
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "192.0.2.25 transport.test"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: |
      192.0.2.25 transport.test
      {large_hosts}
outbound: []
rule_set: []
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
clients: []
"#,
            ))
            .expect("cross-transport fixture must be valid")
            .resolved
    }

    #[tokio::test]
    async fn udp_tcp_and_plain_doh_follow_the_same_dns_contract() {
        let ports = available_transport_ports();
        let config = cross_transport_runtime_config(ports[0], ports[1], ports[2]);
        let prepared =
            PreparedRuntime::prepare_with_policy_core(config, RuntimeRevision(1)).unwrap();
        let factory = SystemSocketFactory::new();
        let bound = crate::runtime::bind_prepared(
            prepared,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(bound));
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();

        let positive = assert_cross_transport_contract(
            query_all_transports(ports, 41, "transport.test.", RecordType::A).await,
            41,
            "transport.test.",
            RecordType::A,
            ResponseClass::Positive,
        );
        assert!(positive.as_message().answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0 == Ipv4Addr::new(192, 0, 2, 25)
        )));

        assert_cross_transport_contract(
            query_all_transports(ports, 51, "transport.test.", RecordType::AAAA).await,
            51,
            "transport.test.",
            RecordType::AAAA,
            ResponseClass::NoData,
        );
        assert_cross_transport_contract(
            query_all_transports(ports, 61, "missing.transport.test.", RecordType::A).await,
            61,
            "missing.transport.test.",
            RecordType::A,
            ResponseClass::NxDomain,
        );

        let [udp, tcp, doh_post, doh_get] =
            query_all_transports(ports, 71, "large.transport.test.", RecordType::A).await;
        assert_eq!(udp.metadata.id, 71);
        assert!(udp.metadata.truncation);
        assert_eq!(udp.metadata.response_code, ResponseCode::NoError);
        let tcp = canonicalize_response(tcp, 72, "large.transport.test.", RecordType::A);
        let doh_post = canonicalize_response(doh_post, 73, "large.transport.test.", RecordType::A);
        let doh_get = canonicalize_response(doh_get, 74, "large.transport.test.", RecordType::A);
        assert_eq!(tcp, doh_post);
        assert_eq!(tcp, doh_get);
        assert_eq!(tcp.class(), ResponseClass::Positive);
        assert!(!tcp.as_message().metadata.truncation);
        assert_eq!(tcp.as_message().answers.len(), 64);

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
    }

    #[tokio::test]
    async fn udp_tcp_and_plain_doh_share_error_response_contract() {
        let ports = available_transport_ports();
        let config = cross_transport_runtime_config(ports[0], ports[1], ports[2]);
        let prepared = PreparedRuntime::prepare(config, RuntimeRevision(2)).unwrap();
        let factory = SystemSocketFactory::new();
        let bound = crate::runtime::bind_prepared(
            prepared,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(bound));
        let mut service = super::DnsService::start_with_coordinator(
            Arc::clone(&coordinator),
            Arc::new(CrossTransportErrorCore),
            Duration::from_secs(5),
        )
        .unwrap();

        assert_cross_transport_contract(
            query_all_transports(ports, 81, "error.transport.test.", RecordType::A).await,
            81,
            "error.transport.test.",
            RecordType::A,
            ResponseClass::ServFail,
        );
        assert_cross_transport_contract(
            query_all_transports(ports, 91, "error.transport.test.", RecordType::AAAA).await,
            91,
            "error.transport.test.",
            RecordType::AAAA,
            ResponseClass::Refused,
        );

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
    }

    /// 验证无流量 deadline 只触发 listener 轮询，不消耗 endpoint 重试预算。
    #[tokio::test]
    async fn idle_listener_deadlines_do_not_exhaust_transport_tasks() {
        let ports = available_transport_ports();
        let config = cross_transport_runtime_config(ports[0], ports[1], ports[2]);
        let prepared =
            PreparedRuntime::prepare_with_policy_core(config, RuntimeRevision(3)).unwrap();
        let factory = SystemSocketFactory::new();
        let bound = crate::runtime::bind_prepared(
            prepared,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(bound));
        let core = coordinator.load().snapshot().dns_core().unwrap();
        let mut service = super::DnsService::start_with_coordinator(
            Arc::clone(&coordinator),
            core,
            Duration::from_millis(50),
        )
        .unwrap();

        // 等待时间超过四轮 deadline；旧行为会在此期间耗尽三次重试。
        tokio::time::sleep(Duration::from_millis(350)).await;
        let responses = query_all_transports(ports, 101, "transport.test.", RecordType::A).await;
        assert_cross_transport_contract(
            responses,
            101,
            "transport.test.",
            RecordType::A,
            ResponseClass::Positive,
        );

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
        assert_eq!(report.restarted, 0);
    }

    #[tokio::test]
    async fn reload_prepared_rebinds_listener_tasks_to_the_new_runtime() {
        let base_port = 40_000 + (std::process::id() as u16 % 1_000) * 2;
        let work_path = crate::config::test_support::absolute_path("service-reload-rebind");
        let initial = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, base_port),
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
            runtime_config_at(&work_path, base_port + 1),
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
            .await
            .unwrap();
        assert!(!report.deadline_expired);
    }

    #[tokio::test]
    async fn reload_bind_failure_keeps_previous_runtime_and_listener_available() {
        let initial_port = 41_000 + (std::process::id() as u16 % 500);
        let occupied = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let blocked_port = occupied.local_addr().unwrap().port();
        let work_path = crate::config::test_support::absolute_path("service-reload-bind-failure");
        let initial = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, initial_port),
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
        let current = coordinator.load();
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();
        let current_address = current.listeners().local_addrs().unwrap()[0];

        let prepared = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, blocked_port),
            RuntimeRevision(2),
        )
        .unwrap();
        let error = service
            .reload_prepared(
                prepared,
                &factory,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, super::ServiceReloadError::Bind(_)));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
        assert_eq!(service.runtime().revision(), RuntimeRevision(1));
        assert!(!current.is_draining());
        assert_eq!(service.transport_task_count(), 1);
        let response = udp_query(current_address, 7, "example.test.").await;
        assert_eq!(response.metadata.id, 7);
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);

        drop(occupied);
        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
    }

    #[tokio::test]
    async fn service_shutdown_flushes_storage_before_closing_telemetry() {
        let port = 40_250 + (std::process::id() as u16 % 500);
        let work_path =
            crate::config::test_support::absolute_path("service-shutdown-storage-telemetry");
        let config = runtime_config_at(&work_path, port);
        let database_path = config.database.path.clone();
        let initial =
            PreparedRuntime::prepare_with_policy_core(Arc::clone(&config), RuntimeRevision(1))
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
        let storage = StorageRuntime::open(
            coordinator.load().snapshot().config(),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .unwrap();
        let output = Arc::new(CountingTelemetryOutput::default());
        let telemetry = Arc::new(TelemetryWriter::new(16, output.clone()).unwrap());
        let telemetry_probe = Arc::clone(&telemetry);
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator_storage_and_telemetry(
                Arc::clone(&coordinator),
                storage,
                telemetry,
            )
            .unwrap();

        assert_eq!(service.task_count(), 3);
        let response = udp_query(
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            1,
            "example.test.",
        )
        .await;
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);
        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
        assert!(telemetry_probe.stats().closed());
        {
            let health_events = output.health_events.lock().unwrap();
            assert!(health_events.iter().any(|event| {
                event.component == TelemetryComponent::Storage
                    && event.state == ComponentHealthState::Stopping
            }));
            assert!(health_events.iter().any(|event| {
                event.component == TelemetryComponent::Telemetry
                    && event.state == ComponentHealthState::Stopping
            }));
        }

        assert_eq!(sqlite_total_requests(&database_path).await, 1);
        let _ = std::fs::remove_dir_all(work_path);
    }

    #[tokio::test]
    async fn reload_prepared_reuses_unchanged_listener_without_rebinding() {
        let port = 40_500 + (std::process::id() as u16 % 500);
        let work_path = crate::config::test_support::absolute_path("service-reload-reuse");
        let initial = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, port),
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
        let initial_response = udp_query(
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            1,
            "example.test.",
        )
        .await;
        assert!(initial_response.answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0 == Ipv4Addr::new(127, 0, 0, 1)
        )));

        let prepared = PreparedRuntime::prepare_with_policy_core(
            runtime_config_with_answer_at(&work_path, port, "127.0.0.2"),
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
        assert_eq!(active.listeners().local_addrs().unwrap()[0].port(), port);
        assert_eq!(service.transport_task_count(), 1);
        assert_eq!(service.resource_task_count(), 0);
        let reloaded_response = udp_query(
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            2,
            "example.test.",
        )
        .await;
        assert!(reloaded_response.answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0 == Ipv4Addr::new(127, 0, 0, 2)
        )));

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
    }

    /// 验证 Listener health 跨启动、降级、成功 reload 和 shutdown 完成闭环。
    #[tokio::test]
    async fn listener_health_recovers_on_reload_and_stops_on_shutdown() {
        let port = 40_750 + (std::process::id() as u16 % 500);
        let work_path = crate::config::test_support::absolute_path("service-listener-health");
        let initial = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, port),
            RuntimeRevision(1),
        )
        .unwrap();
        let factory = SystemSocketFactory::new();
        let initial = crate::runtime::bind_prepared(
            initial,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(initial));
        let core = coordinator.load().snapshot().dns_core().unwrap();
        let output = Arc::new(CountingTelemetryOutput::default());
        let telemetry = Arc::new(TelemetryWriter::new(16, output.clone()).unwrap());
        let mut service = super::DnsService::start_with_optional_storage_and_telemetry(
            Arc::clone(&coordinator),
            core,
            Duration::from_secs(5),
            None,
            Some(Arc::clone(&telemetry)),
        )
        .unwrap();
        publish_component_health(
            &telemetry,
            TelemetryComponent::Listener,
            ComponentHealthState::Degraded,
            Some("test listener degradation"),
        );

        let prepared = PreparedRuntime::prepare_with_policy_core(
            runtime_config_with_answer_at(&work_path, port, "127.0.0.2"),
            RuntimeRevision(2),
        )
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
        service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();

        let listener_states = output
            .health_events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.component == TelemetryComponent::Listener)
            .map(|event| event.state)
            .collect::<Vec<_>>();
        assert_eq!(
            listener_states,
            vec![
                ComponentHealthState::Healthy,
                ComponentHealthState::Degraded,
                ComponentHealthState::Healthy,
                ComponentHealthState::Stopping,
            ]
        );
        let _ = std::fs::remove_dir_all(work_path);
    }

    #[tokio::test]
    async fn reload_reuses_process_owned_stats_worker_for_the_new_core() {
        let port = 41_000 + (std::process::id() as u16 % 500);
        let work_path = crate::config::test_support::absolute_path("service-reload-shared-stats");
        let config = runtime_config_at(&work_path, port);
        let initial =
            PreparedRuntime::prepare_with_policy_core(Arc::clone(&config), RuntimeRevision(1))
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
        let storage = StorageRuntime::open(
            coordinator.load().snapshot().config(),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .unwrap();
        let mut service = super::DnsService::with_default_timeout_from_coordinator_and_storage(
            Arc::clone(&coordinator),
            storage,
        )
        .unwrap();
        let stats_worker = Arc::clone(service.stats_worker.as_ref().unwrap());
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        // 先让 interval 的启动 tick 完成，避免它在第一条请求后立即切换统计 epoch。
        tokio::time::sleep(Duration::from_millis(20)).await;

        let initial_response = udp_query(address, 1, "example.test.").await;
        assert_eq!(initial_response.metadata.id, 1);
        assert_eq!(stats_worker.accumulator().active_event_count(), 1);

        let prepared = PreparedRuntime::prepare_with_policy_core(
            runtime_config_with_answer_at(&work_path, port, "127.0.0.2"),
            RuntimeRevision(2),
        )
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

        assert!(Arc::ptr_eq(
            &stats_worker,
            service.stats_worker.as_ref().unwrap()
        ));
        let reloaded_response = udp_query(address, 2, "example.test.").await;
        assert_eq!(reloaded_response.metadata.id, 2);
        assert!(reloaded_response.answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0 == Ipv4Addr::new(127, 0, 0, 2)
        )));
        assert_eq!(stats_worker.accumulator().active_event_count(), 2);

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
        let _ = std::fs::remove_dir_all(work_path);
    }

    #[tokio::test]
    async fn reload_rejects_process_owned_config_change_without_switching_runtime() {
        let port = 45_000 + (std::process::id() as u16 % 500);
        let work_path = crate::config::test_support::absolute_path("service-reload-restart");
        let initial = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, port),
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
        let current = coordinator.load();
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();

        let mut candidate_config = Arc::try_unwrap(runtime_config_at(&work_path, port)).unwrap();
        candidate_config.database.path = candidate_config.work.path.join("other.sqlite");
        let prepared = PreparedRuntime::prepare_with_policy_core(
            Arc::new(candidate_config),
            RuntimeRevision(2),
        )
        .unwrap();
        let error = service
            .reload_prepared(
                prepared,
                &factory,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            super::ServiceReloadError::RestartRequired {
                component: "database"
            }
        ));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
        assert!(!current.is_draining());
        assert_eq!(service.runtime().revision(), RuntimeRevision(1));

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
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
    async fn running_service_observes_published_resource_refresh() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-service-live-resource-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let resource_path = root.join("hosts.txt");
        std::fs::write(&resource_path, "192.0.2.10 old.example\n").unwrap();
        let port = 43_000 + (std::process::id() as u16 % 500);
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
        let coordinator = Arc::new(RuntimeCoordinator::new(initial));
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();
        let address = service.runtime().listeners().local_addrs().unwrap()[0];

        let initial_response = udp_query(address, 1, "old.example.").await;
        assert_eq!(initial_response.metadata.id, 1);
        assert_eq!(
            initial_response.metadata.response_code,
            ResponseCode::NoError
        );
        assert!(initial_response.answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0 == Ipv4Addr::new(192, 0, 2, 10)
        )));

        std::fs::write(&resource_path, "192.0.2.11 new.example\n").unwrap();
        let resource = crate::config::resolve::ConfigId::new("local-hosts").unwrap();
        coordinator
            .refresh_resource(
                &resource,
                u64::MAX,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();

        let refreshed_response = udp_query(address, 2, "new.example.").await;
        assert_eq!(refreshed_response.metadata.id, 2);
        assert_eq!(
            refreshed_response.metadata.response_code,
            ResponseCode::NoError
        );
        assert!(refreshed_response.answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0 == Ipv4Addr::new(192, 0, 2, 11)
        )));

        let report = service
            .shutdown(
                &SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shutdown_closes_finalizers_from_previous_and_current_runtime() {
        let port = 41_500 + (std::process::id() as u16 % 500);
        let work_path = crate::config::test_support::absolute_path("service-reload-finalizer");
        let initial = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, port),
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
        let old_finalizer = initial.snapshot().policy_core().unwrap().finalizer_owner();
        old_finalizer
            .submit_task(std::future::pending::<()>())
            .unwrap();
        let coordinator = Arc::new(RuntimeCoordinator::new(initial));
        let mut service =
            super::DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator))
                .unwrap();

        let prepared = PreparedRuntime::prepare_with_policy_core(
            runtime_config_at(&work_path, port),
            RuntimeRevision(2),
        )
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
            .await
            .unwrap();
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
            .await
            .unwrap();
        assert!(!report.deadline_expired);
        let _ = std::fs::remove_dir_all(root);
    }
}
