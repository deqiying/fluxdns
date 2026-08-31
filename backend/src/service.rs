//! Application 使用的 DNS service task 编排。

use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::task::JoinSet;

use crate::config::BindTransport;
use crate::dns::{
    CacheCompatibilityKey, CancelReason, Cancellation, DispatchError, DnsCore,
    TransportCapabilities, TransportClass, dispatch_inbound,
};
use crate::ports::PortErrorClass;
use crate::ports::effects::{ActivatedSocketHandle, Clock};
use crate::ports::inbound::InboundAdapter;
use crate::runtime::{
    ActiveRuntime, AdmissionError, BoundEndpointHandle, FaultLevel, RestartPolicy, ShutdownReport,
    Supervisor, SupervisorError, SystemClock, TaskError, TaskSpec,
};
use crate::transport::doh::{DohAdapter, DohAdapterError, DohSession, DohSessionEvent};
use crate::transport::{
    DEFAULT_REQUEST_TIMEOUT, TcpAdapter, TcpAdapterError, TcpSession, UdpAdapter, UdpAdapterError,
};

#[derive(Debug, Error)]
pub enum ServiceStartError {
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
}

/// 已绑定 listener 的 DNS service；所有 receive loop 都由同一个 Supervisor 持有。
pub struct DnsService {
    runtime: Arc<ActiveRuntime>,
    supervisor: Supervisor,
}

impl DnsService {
    pub fn start(
        runtime: Arc<ActiveRuntime>,
        core: Arc<dyn DnsCore>,
        request_timeout: Duration,
    ) -> Result<Self, ServiceStartError> {
        let endpoints = runtime.listeners().endpoint_handles().map_err(|error| {
            ServiceStartError::ListenerHandles {
                class: error.class().as_str(),
                operation: error.operation(),
            }
        })?;
        let mut supervisor = Supervisor::new();
        let transport_cancellation = supervisor.cancellation();

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
                let task = doh_listener_task(
                    adapter,
                    Arc::clone(&core),
                    Arc::clone(&runtime),
                    transport_cancellation.clone(),
                );
                spawn_task(&mut supervisor, task_id, "doh", task)?;
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
                    let task = service_task(
                        adapter,
                        Arc::clone(&core),
                        Arc::clone(&runtime),
                        transport_cancellation.clone(),
                    );
                    spawn_task(&mut supervisor, task_id, "udp", task)?;
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
                    let task = tcp_listener_task(
                        adapter,
                        Arc::clone(&core),
                        Arc::clone(&runtime),
                        transport_cancellation.clone(),
                    );
                    spawn_task(&mut supervisor, task_id, "tcp", task)?;
                }
            }
        }

        Ok(Self {
            runtime,
            supervisor,
        })
    }

    pub fn with_default_timeout(
        runtime: Arc<ActiveRuntime>,
        core: Arc<dyn DnsCore>,
    ) -> Result<Self, ServiceStartError> {
        Self::start(runtime, core, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn runtime(&self) -> &Arc<ActiveRuntime> {
        &self.runtime
    }

    pub fn task_count(&self) -> usize {
        self.supervisor.task_count()
    }

    pub async fn shutdown(
        &mut self,
        clock: &dyn Clock,
        deadline: crate::dns::Deadline,
    ) -> ShutdownReport {
        self.runtime.begin_drain();
        self.supervisor.shutdown(clock, deadline).await
    }

    /// 等待 Ctrl-C 后执行有界 graceful shutdown。
    pub async fn wait_for_ctrl_c(
        &mut self,
        grace_period: Duration,
    ) -> Result<ShutdownReport, ServiceError> {
        tokio::signal::ctrl_c()
            .await
            .map_err(|_| ServiceError::Signal)?;
        let deadline = crate::dns::Deadline::new(Instant::now() + grace_period);
        let report = self.shutdown(&SystemClock::new(), deadline).await;
        if report.deadline_expired {
            Err(ServiceError::ShutdownDeadline)
        } else {
            Ok(report)
        }
    }
}

fn capabilities(class: TransportClass) -> TransportCapabilities {
    TransportCapabilities {
        class,
        cache_compatibility: CacheCompatibilityKey(1),
    }
}

fn spawn_task(
    supervisor: &mut Supervisor,
    task_id: String,
    component: &'static str,
    task: crate::runtime::TaskFuture,
) -> Result<(), ServiceStartError> {
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
        .spawn(spec, task)
        .map_err(ServiceStartError::Task)
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
    use super::capabilities;
    use crate::dns::{CacheCompatibilityKey, TransportClass};

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
}
