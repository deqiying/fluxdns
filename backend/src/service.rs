//! Application 使用的 DNS service task 编排。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::task::JoinSet;

use crate::config::BindTransport;
use crate::config::resolve::ConfigId;
use crate::dns::{
    CacheCompatibilityKey, CancelReason, Cancellation, Deadline, DispatchError, DnsCore,
    TransportCapabilities, TransportClass, dispatch_inbound,
};
use crate::ports::PortErrorClass;
use crate::ports::effects::{ActivatedSocketHandle, Clock};
use crate::ports::inbound::InboundAdapter;
use crate::runtime::{
    ActiveRuntime, AdmissionError, BoundEndpointHandle, FaultLevel, RefreshedResourceSnapshot,
    ResourceRefreshCoordinatorError, RestartPolicy, RuntimeCoordinator, ShutdownReport, Supervisor,
    SupervisorError, SystemClock, TaskCompletion, TaskError, TaskErrorKind, TaskExit, TaskSpec,
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

/// 已绑定 listener 的 DNS service；所有 receive loop 都由同一个 Supervisor 持有。
pub struct DnsService {
    runtime: Arc<ActiveRuntime>,
    coordinator: Arc<RuntimeCoordinator>,
    supervisor: Supervisor,
    transport_cancellations: Vec<Cancellation>,
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
        let endpoints = runtime.listeners().endpoint_handles().map_err(|error| {
            ServiceStartError::ListenerHandles {
                class: error.class().as_str(),
                operation: error.operation(),
            }
        })?;
        let mut supervisor = Supervisor::new();
        let resource_cancellation = supervisor.cancellation();
        let mut transport_cancellations = Vec::new();

        for (index, endpoint) in endpoints.into_iter().enumerate() {
            let BoundEndpointHandle { entry, socket } = endpoint;
            if entry.transport == BindTransport::Doh {
                let adapter = DohAdapter::from_endpoint(
                    BoundEndpointHandle { entry, socket },
                    runtime.snapshot().config(),
                    runtime.revision(),
                    capabilities(TransportClass::Multiplexed),
                    request_timeout,
                )
                .map_err(|reason| ServiceStartError::Endpoint {
                    index,
                    kind: "DoH",
                    reason: reason.to_string(),
                })?;
                let task_id = format!("transport.doh.{index}");
                let task_core = Arc::clone(&core);
                let task_runtime = Arc::clone(&runtime);
                let cancellation =
                    spawn_transport_task(&mut supervisor, task_id, "doh", move |cancellation| {
                        doh_listener_task(adapter, task_core, task_runtime, cancellation)
                    })?;
                transport_cancellations.push(cancellation);
                continue;
            }
            match socket {
                ActivatedSocketHandle::Udp(socket) => {
                    let adapter = UdpAdapter::from_endpoint(
                        BoundEndpointHandle {
                            entry,
                            socket: ActivatedSocketHandle::Udp(socket),
                        },
                        runtime.revision(),
                        capabilities(TransportClass::Datagram),
                        request_timeout,
                    )
                    .map_err(|reason| ServiceStartError::Endpoint {
                        index,
                        kind: "UDP",
                        reason: reason.to_string(),
                    })?;
                    let task_id = format!("transport.udp.{index}");
                    let task_core = Arc::clone(&core);
                    let task_runtime = Arc::clone(&runtime);
                    let cancellation = spawn_transport_task(
                        &mut supervisor,
                        task_id,
                        "udp",
                        move |cancellation| {
                            service_task(adapter, task_core, task_runtime, cancellation)
                        },
                    )?;
                    transport_cancellations.push(cancellation);
                }
                ActivatedSocketHandle::Tcp(listener) => {
                    let adapter = TcpAdapter::from_endpoint(
                        BoundEndpointHandle {
                            entry,
                            socket: ActivatedSocketHandle::Tcp(listener),
                        },
                        runtime.revision(),
                        capabilities(TransportClass::Stream),
                        request_timeout,
                    )
                    .map_err(|reason| ServiceStartError::Endpoint {
                        index,
                        kind: "TCP",
                        reason: reason.to_string(),
                    })?;
                    let task_id = format!("transport.tcp.{index}");
                    let task_core = Arc::clone(&core);
                    let task_runtime = Arc::clone(&runtime);
                    let cancellation = spawn_transport_task(
                        &mut supervisor,
                        task_id,
                        "tcp",
                        move |cancellation| {
                            tcp_listener_task(adapter, task_core, task_runtime, cancellation)
                        },
                    )?;
                    transport_cancellations.push(cancellation);
                }
            }
        }

        for (index, resource) in coordinator.resource_worker_ids().into_iter().enumerate() {
            let task_id = format!("resource.refresh.{index}");
            let task = resource_refresh_task(
                Arc::clone(&coordinator),
                resource,
                resource_cancellation.clone(),
            );
            spawn_resource_task(&mut supervisor, task_id, task)?;
        }

        Ok(Self {
            runtime,
            coordinator,
            supervisor,
            transport_cancellations,
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

    pub async fn shutdown(
        &mut self,
        clock: &dyn Clock,
        deadline: crate::dns::Deadline,
    ) -> ShutdownReport {
        self.runtime.begin_drain();
        self.cancel_transport_tasks();
        let mut report = self.supervisor.shutdown(clock, deadline).await;
        if let Some(core) = self.runtime.snapshot().policy_core()
            && !core.shutdown_until(deadline).await
        {
            report.deadline_expired = true;
        }
        report
    }

    /// 等待 Ctrl-C 后执行有界 graceful shutdown。
    pub async fn wait_for_ctrl_c(
        &mut self,
        grace_period: Duration,
    ) -> Result<ShutdownReport, ServiceError> {
        let signal = tokio::signal::ctrl_c();
        tokio::pin!(signal);
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
            }
        }
    }
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
    F: FnOnce(Cancellation) -> crate::runtime::TaskFuture + Send + 'static,
{
    let spec = TaskSpec::new(
        task_id,
        component,
        FaultLevel::FatalEndpoint,
        RestartPolicy::Never,
    )
    .map_err(|error| ServiceStartError::Endpoint {
        index: 0,
        kind: component,
        reason: error.to_string(),
    })?;
    supervisor
        .spawn_scoped(spec, factory)
        .map_err(ServiceStartError::Task)
}

fn spawn_resource_task(
    supervisor: &mut Supervisor,
    task_id: String,
    task: crate::runtime::TaskFuture,
) -> Result<(), ServiceStartError> {
    let spec = TaskSpec::new(
        task_id,
        "resource",
        FaultLevel::Degraded,
        RestartPolicy::Never,
    )
    .map_err(|error| ServiceStartError::Endpoint {
        index: 0,
        kind: "resource",
        reason: error.to_string(),
    })?;
    supervisor
        .spawn(spec, task)
        .map_err(ServiceStartError::Task)
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
            coordinator.shutdown_resource_refresh();
            return Err(TaskError::Cancelled);
        }
        let now = unix_seconds();
        let runtime = coordinator.load();
        let decision = runtime
            .resource_refresh_decision(&resource, now)
            .ok_or(TaskError::Fatal)?;
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
                    coordinator.shutdown_resource_refresh();
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
            coordinator.shutdown_resource_refresh();
            return Err(TaskError::Cancelled);
        };
        let wait = Duration::from_secs(next_due.saturating_sub(now).max(1));
        tokio::select! {
            _ = cancellation.cancelled() => {
                coordinator.shutdown_resource_refresh();
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
    use super::{ServiceError, capabilities, spawn_transport_task, task_failure};
    use crate::dns::{CacheCompatibilityKey, CancelReason, TransportClass};
    use crate::runtime::{
        FaultLevel, RestartPolicy, Supervisor, TaskCompletion, TaskError, TaskErrorKind, TaskExit,
        TaskSpec,
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
}
