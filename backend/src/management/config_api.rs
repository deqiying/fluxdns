//! 配置写接口：`/config/save`、`/config/reload`、`/config/persist`。
//!
//! 写操作在独立 tokio 任务中执行并等待结果：HTTP 断开不会中断已经开始的 Runtime 切换与写盘，
//! 前端重新读取 `config/state` 即可得知结果，因此不需要 operation ID 与幂等去重。

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use super::config_query::document_state;
use super::contract::{
    ErrorCode, MAX_MUTATION_BYTES, PersistResult, ReloadResult, SaveResult, decode_save,
};
use super::errors::{ManagementError, control_diagnostic};
use super::router::{
    AuthServices, RequestId, v2_error_response, v2_error_response_detailed,
    v2_error_response_with_fields, validate_v2_mutating_request,
};
use crate::config::resolve::ResolvedConfig;
use crate::config::store::{ApplyOutcome, ConfigStore, RuntimeHost, SaveInput, StoreError};
use crate::diagnostics::Diagnostic;
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::runtime::{PreparedRuntime, RuntimeCoordinator};
use crate::service::{ControlError, ServiceControl};

/// 资源准备与 Runtime 切换共用的时间预算。
const APPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// 生产 Runtime 宿主：prepare 候选并通过 service 控制队列切换。
pub(crate) struct ServiceHost {
    control: ServiceControl,
    coordinator: Arc<RuntimeCoordinator>,
}

impl ServiceHost {
    pub(crate) fn new(control: ServiceControl, coordinator: Arc<RuntimeCoordinator>) -> Self {
        Self {
            control,
            coordinator,
        }
    }
}

impl RuntimeHost for ServiceHost {
    fn current(&self) -> (u64, Arc<ResolvedConfig>) {
        let runtime = self.coordinator.load();
        (runtime.revision().0, runtime.snapshot().config_arc())
    }

    fn apply(
        &self,
        expected: u64,
        config: Arc<ResolvedConfig>,
    ) -> impl Future<Output = ApplyOutcome> + Send {
        let control = self.control.clone();
        async move {
            let Some(next) = expected.checked_add(1) else {
                return ApplyOutcome::Failed(Diagnostic::new(
                    "runtime_prepare",
                    "REVISION_EXHAUSTED",
                    "运行配置版本号已达到上限，无法继续应用。",
                ));
            };
            let deadline = Deadline::new(Instant::now() + APPLY_TIMEOUT);
            let prepared = match PreparedRuntime::prepare_with_policy_core_and_remote_resources(
                config,
                RuntimeRevision(next),
                deadline,
                Cancellation::new(),
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(error) => return ApplyOutcome::Failed(error.diagnostic()),
            };
            let receipt = match control.try_apply(RuntimeRevision(expected), prepared, deadline) {
                Ok(receipt) => receipt,
                Err(error) => return ApplyOutcome::Failed(control_diagnostic(&error)),
            };
            match receipt.owner_outcome().await {
                Ok(revision) => ApplyOutcome::Applied(revision.0),
                Err(ControlError::OutcomeUnknown) => ApplyOutcome::Unknown,
                Err(error) => ApplyOutcome::Failed(control_diagnostic(&error)),
            }
        }
    }
}

pub(super) fn routes() -> Router<Arc<AuthServices>> {
    Router::new()
        .route("/api/v2/config/save", post(post_save))
        .route("/api/v2/config/reload", post(post_reload))
        .route("/api/v2/config/persist", post(post_persist))
        .layer(DefaultBodyLimit::max(MAX_MUTATION_BYTES))
}

async fn post_save(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Some(host) = services.config_host.clone() else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let body = match body_bytes(body) {
        Ok(body) => body,
        Err(error) => return v2_error_response(error, &request_id),
    };
    let request = match decode_save(&body) {
        Ok(request) => request,
        Err(error) => {
            return v2_error_response_with_fields(error.code, error.field_errors, &request_id);
        }
    };
    let input = SaveInput {
        changes: request.changes,
        overwrite_disk: request.overwrite_disk,
        confirm_retention_shortening: request.confirm_retention_shortening,
    };
    let result = detached(&services.config_store, move |store| async move {
        let outcome = store.save(&*host, input).await?;
        tracing::info!(
            event = "configuration_saved",
            component = "management",
            persisted = outcome.persisted,
            runtime_revision = outcome.document.runtime_revision,
            "configuration_saved"
        );
        Ok(SaveResult {
            state: document_state(&store, &outcome.document)?,
            persisted: outcome.persisted,
            diagnostic: outcome.diagnostic,
        })
    })
    .await;
    respond(result, &request_id)
}

async fn post_reload(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Some(host) = services.config_host.clone() else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let result = detached(&services.config_store, move |store| async move {
        let document = store.reload(&*host).await?;
        tracing::info!(
            event = "configuration_reloaded",
            component = "management",
            runtime_revision = document.runtime_revision,
            "configuration_reloaded"
        );
        Ok(ReloadResult {
            state: document_state(&store, &document)?,
        })
    })
    .await;
    respond(result, &request_id)
}

async fn post_persist(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let result = detached(&services.config_store, |store| async move {
        let document = store.persist().await?;
        tracing::info!(
            event = "configuration_persisted",
            component = "management",
            "configuration_persisted"
        );
        Ok(PersistResult {
            state: document_state(&store, &document)?,
        })
    })
    .await;
    respond(result, &request_id)
}

/// 在独立任务中执行写操作并等待结果；请求被取消时任务继续运行到结束。
async fn detached<T, F, Fut>(store: &Arc<ConfigStore>, work: F) -> Result<T, ManagementError>
where
    T: Send + 'static,
    F: FnOnce(Arc<ConfigStore>) -> Fut,
    Fut: Future<Output = Result<T, ManagementError>> + Send + 'static,
{
    tokio::spawn(work(Arc::clone(store)))
        .await
        .map_err(|_| ManagementError::from(StoreError::Internal))?
}

fn respond<T: serde::Serialize>(
    result: Result<T, ManagementError>,
    request_id: &RequestId,
) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => v2_error_response_detailed(error, request_id),
    }
}

fn body_bytes(body: Result<Bytes, BytesRejection>) -> Result<Bytes, ErrorCode> {
    body.map_err(|error| {
        if error.into_response().status() == StatusCode::PAYLOAD_TOO_LARGE {
            ErrorCode::PayloadTooLarge
        } else {
            ErrorCode::InvalidArgument
        }
    })
}

#[cfg(test)]
mod tests;
