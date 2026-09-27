//! 独立 HTTP Management listener 与 Supervisor task 适配。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use thiserror::Error;

use super::ManagementRuntime;
use super::assets;
use super::auth::{AuthError, AuthState};
use super::events::EventHub;
use super::metrics::MetricsOwner;
use super::query::{ManagementHistoryDependencies, ManagementQueryService};
use super::router::{AuthServices, build_router};
use super::session::SessionStore;
use crate::config::resolve::ResolvedWebUi;
use crate::config::store::ConfigStore;
use crate::dns::Cancellation;
use crate::runtime::{RuntimeCoordinator, TaskError};
use crate::service::ServiceControl;

pub(crate) struct ManagementService {
    listener: tokio::net::TcpListener,
    router: Router,
    runtime: Arc<ManagementRuntime>,
    events: Option<Arc<EventHub>>,
}

pub(crate) struct ManagementQueryDependencies {
    coordinator: Arc<RuntimeCoordinator>,
    metrics: Arc<MetricsOwner>,
    history: ManagementHistoryDependencies,
}

impl ManagementQueryDependencies {
    pub(crate) fn new(
        coordinator: Arc<RuntimeCoordinator>,
        metrics: Arc<MetricsOwner>,
        history: ManagementHistoryDependencies,
    ) -> Self {
        Self {
            coordinator,
            metrics,
            history,
        }
    }
}

impl ManagementService {
    /// 生产 v2 启动传入已冻结活动源的 ConfigStore，确保只读投影与运行态同源。
    pub(crate) async fn bind_with_config_store(
        config: &ResolvedWebUi,
        config_store: Arc<ConfigStore>,
        config_control: ServiceControl,
        dependencies: ManagementQueryDependencies,
    ) -> Result<Self, ManagementBuildError> {
        assets::ensure_available().map_err(ManagementBuildError::Assets)?;
        let origin = config
            .public_origin
            .as_ref()
            .ok_or(ManagementBuildError::MissingPublicOrigin)?;
        let auth = Arc::new(AuthState::new(&config.users).map_err(ManagementBuildError::Auth)?);
        let sessions = Arc::new(SessionStore::new(origin.scheme() == "https"));
        let metrics = Arc::clone(&dependencies.metrics);
        let queries = Arc::new(ManagementQueryService::new(
            dependencies.coordinator,
            dependencies.metrics,
            dependencies.history,
        ));
        let events = Arc::new(
            EventHub::new(
                Arc::clone(&sessions),
                metrics,
                Arc::clone(&queries),
                Arc::clone(&config_store),
            )
            .map_err(ManagementBuildError::Events)?,
        );
        let services = Arc::new(
            AuthServices::new(
                Arc::clone(&auth),
                Arc::clone(&sessions),
                Arc::clone(&config_store),
                origin.as_str().trim_end_matches('/').to_owned(),
                Some(queries),
            )
            .with_config_control(config_control)
            .with_events(Arc::clone(&events)),
        );
        let runtime = Arc::new(ManagementRuntime::new(
            auth,
            sessions,
            config_store,
            Some(Arc::clone(&events)),
        ));
        let address = SocketAddr::new(config.address, config.port);
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(ManagementBuildError::Bind)?;
        Ok(Self {
            listener,
            router: build_router(services),
            runtime,
            events: Some(events),
        })
    }

    pub(crate) fn runtime(&self) -> Arc<ManagementRuntime> {
        Arc::clone(&self.runtime)
    }

    #[cfg(test)]
    fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.listener.local_addr()
    }

    pub(crate) async fn serve(self, cancellation: Cancellation) -> Result<(), TaskError> {
        let collector = self
            .events
            .as_ref()
            .and_then(|events| events.start_collector());
        let shutdown_cancellation = cancellation.clone();
        let shutdown_events = self.events.clone();
        let shutdown = async move {
            shutdown_cancellation.cancelled().await;
            if let Some(events) = shutdown_events {
                events.shutdown();
            }
        };
        let result = axum::serve(
            self.listener,
            self.router
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await;
        if let Some(events) = &self.events {
            events.shutdown();
        }
        if let Some(collector) = collector {
            let _ = collector.await;
        }
        if cancellation.is_cancelled() {
            Err(TaskError::Cancelled)
        } else {
            result.map_err(|_| TaskError::Fatal)?;
            Err(TaskError::Fatal)
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum ManagementBuildError {
    #[error("management public origin is missing")]
    MissingPublicOrigin,
    #[error("management authentication initialization failed")]
    Auth(#[source] AuthError),
    #[error("management WebSocket initialization failed: {0}")]
    Events(&'static str),
    #[error("management WebUI assets are unavailable: {0}")]
    Assets(&'static str),
    #[error("management HTTP listener bind failed")]
    Bind(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{ManagementService, build_router};
    use crate::config::hash::deterministic_hash;
    use crate::config::store::ConfigStore;
    use crate::dns::Cancellation;
    use crate::management::ManagementRuntime;
    use crate::management::auth::AuthState;
    use crate::management::router::AuthServices;
    use crate::management::session::SessionStore;
    use crate::runtime::TaskError;

    #[tokio::test]
    async fn listener_serves_plain_http_and_stops_on_cancellation() {
        let root = crate::config::test_support::absolute_path("management-http-listener");
        std::fs::create_dir_all(&root).unwrap();
        let source_path = std::path::Path::new(&root).join("config.yaml");
        std::fs::write(&source_path, "version: 1\n").unwrap();
        let auth = Arc::new(AuthState::new(&[]).unwrap());
        let sessions = Arc::new(SessionStore::new(false));
        let config_store = Arc::new(ConfigStore::new(
            source_path.clone(),
            source_path,
            deterministic_hash(b"version: 1\n"),
        ));
        let services = Arc::new(AuthServices::new(
            Arc::clone(&auth),
            Arc::clone(&sessions),
            Arc::clone(&config_store),
            "http://127.0.0.1".to_owned(),
            None,
        ));
        let service = ManagementService {
            listener: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            router: build_router(services),
            runtime: Arc::new(ManagementRuntime::new(auth, sessions, config_store, None)),
            events: None,
        };
        let address = service.local_addr().unwrap();
        let cancellation = Cancellation::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move { service.serve(task_cancellation).await });

        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                b"GET /api/v2/auth/setup HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));

        cancellation.cancel(crate::dns::CancelReason::Shutdown);
        assert!(matches!(task.await.unwrap(), Err(TaskError::Cancelled)));
        let _ = std::fs::remove_dir_all(root);
    }
}
