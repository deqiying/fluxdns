//! 独立 HTTP Management listener 与 Supervisor task 适配。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use thiserror::Error;

use super::ManagementRuntime;
use super::assets;
use super::auth::{AuthError, AuthState};
use super::query::ManagementQueryService;
use super::router::{AuthServices, build_router};
use super::session::SessionStore;
use crate::config::resolve::ResolvedWebUi;
use crate::config::store::ConfigStore;
use crate::dns::Cancellation;
use crate::observability::TelemetryWriter;
use crate::ports::management::ManagementStorageRead;
use crate::resolution::ResolutionPipelineMetrics;
use crate::runtime::{RuntimeCoordinator, TaskError};
use crate::storage::{SqliteManagementReadModel, SqliteManagementReadModelBuildError};

pub(crate) struct ManagementService {
    listener: tokio::net::TcpListener,
    router: Router,
    runtime: Arc<ManagementRuntime>,
}

pub(crate) struct ManagementQueryDependencies {
    coordinator: Arc<RuntimeCoordinator>,
    database_path: PathBuf,
    resolve_log_enabled: bool,
    telemetry: Option<Arc<TelemetryWriter>>,
    resolution_metrics: Arc<ResolutionPipelineMetrics>,
}

impl ManagementQueryDependencies {
    pub(crate) fn new(
        coordinator: Arc<RuntimeCoordinator>,
        database_path: PathBuf,
        resolve_log_enabled: bool,
        telemetry: Option<Arc<TelemetryWriter>>,
        resolution_metrics: Arc<ResolutionPipelineMetrics>,
    ) -> Self {
        Self {
            coordinator,
            database_path,
            resolve_log_enabled,
            telemetry,
            resolution_metrics,
        }
    }
}

impl ManagementService {
    pub(crate) async fn bind(
        config: &ResolvedWebUi,
        source_path: PathBuf,
        snapshot_path: PathBuf,
        source_fingerprint: String,
        dependencies: ManagementQueryDependencies,
    ) -> Result<Self, ManagementBuildError> {
        assets::ensure_available().map_err(ManagementBuildError::Assets)?;
        let origin = config
            .public_origin
            .as_ref()
            .ok_or(ManagementBuildError::MissingPublicOrigin)?;
        let auth = Arc::new(AuthState::new(&config.users).map_err(ManagementBuildError::Auth)?);
        let sessions = Arc::new(SessionStore::new(origin.scheme() == "https"));
        let config_store = Arc::new(ConfigStore::new(
            source_path,
            snapshot_path,
            source_fingerprint,
        ));
        let read_model: Arc<dyn ManagementStorageRead> = Arc::new(
            SqliteManagementReadModel::connect(dependencies.database_path)
                .await
                .map_err(ManagementBuildError::ReadModel)?,
        );
        let queries = Arc::new(ManagementQueryService::new(
            dependencies.coordinator,
            read_model,
            dependencies.telemetry,
            dependencies.resolve_log_enabled,
            dependencies.resolution_metrics,
        ));
        let services = Arc::new(AuthServices::new(
            Arc::clone(&auth),
            Arc::clone(&sessions),
            Arc::clone(&config_store),
            origin.as_str().trim_end_matches('/').to_owned(),
            Some(queries),
        ));
        let runtime = Arc::new(ManagementRuntime::new(auth, sessions, config_store));
        let address = SocketAddr::new(config.address, config.port);
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(ManagementBuildError::Bind)?;
        Ok(Self {
            listener,
            router: build_router(services),
            runtime,
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
        let shutdown_cancellation = cancellation.clone();
        let shutdown = async move {
            shutdown_cancellation.cancelled().await;
        };
        let result = axum::serve(
            self.listener,
            self.router
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await;
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
    #[error("management read model initialization failed")]
    ReadModel(#[source] SqliteManagementReadModelBuildError),
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
    use crate::config::migrate::deterministic_hash;
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
            runtime: Arc::new(ManagementRuntime::new(auth, sessions, config_store)),
        };
        let address = service.local_addr().unwrap();
        let cancellation = Cancellation::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move { service.serve(task_cancellation).await });

        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                b"GET /api/v1/auth/setup HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
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
