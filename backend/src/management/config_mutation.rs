//! P1 配置事务 owner：连接活动源、Runtime prepare、service 控制回执与应用后持久化。

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::config::edit::{ConfigChange, ResourceMutation};
use crate::config::store::ConfigStore;
use crate::config::store::active::{
    ActiveError, BeginApply, ExpectedRevisions, Impact, OperationFailure,
};
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::runtime::PreparedRuntime;
use crate::service::{ServiceControl, ServiceReloadError};

use super::config_query::{error_code, operation_result};
use super::contract::{
    ApplyRequest, Candidate, ErrorCode, FileSyncRequest, ImpactKind, OperationResult,
    OperationStatus, Preconditions, Revision, ValidationResult, decode_apply, decode_candidate,
    decode_file_sync, decode_module_candidate,
};
use super::router::{AuthServices, RequestId, v2_error_response, validate_v2_mutating_request};
use super::session::SessionView;

const APPLY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct ConfigMutationOwner {
    store: Arc<ConfigStore>,
    control: ServiceControl,
}

impl ConfigMutationOwner {
    pub(super) fn new(store: Arc<ConfigStore>, control: ServiceControl) -> Self {
        Self { store, control }
    }

    /// 校验可能解析 2 MiB 候选，因此在阻塞线程执行，不占用 Management executor。
    pub(super) async fn validate(
        &self,
        actor: String,
        candidate: Candidate,
    ) -> Result<ValidationResult, ErrorCode> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let expected = expected(&candidate.expected);
            let validated = store
                .validate_edit(
                    &actor,
                    &expected,
                    &candidate.changes,
                    candidate.discard_external_changes,
                )
                .map_err(error_code)?;
            Ok(ValidationResult {
                validation_token: revision(validated.token)?,
                expected: candidate.expected,
                expires_at_ms: expires_at_millis(validated.expires_in)?,
                required_confirmations: validated.impacts.into_iter().map(impact_kind).collect(),
                affected_names: affected_names(&candidate.changes),
            })
        })
        .await
        .map_err(|_| ErrorCode::ServiceUnavailable)?
    }

    /// 受理成功即返回可查询的 operation；后续 prepare、应用和持久化不依赖 HTTP 生命周期。
    pub(super) async fn start_apply(
        &self,
        actor: String,
        request: ApplyRequest,
    ) -> Result<OperationResult, ErrorCode> {
        let store = Arc::clone(&self.store);
        let begin_actor = actor.clone();
        let operation_id = request.operation_id.as_str().to_owned();
        let query_operation_id = operation_id.clone();
        let candidate = request.candidate;
        let validation_token = request.validation_token.as_str().to_owned();
        let confirmations = request
            .confirmations
            .into_iter()
            .map(impact)
            .collect::<BTreeSet<_>>();
        let begin = tokio::task::spawn_blocking(move || {
            store.begin_apply(
                &begin_actor,
                &operation_id,
                &expected(&candidate.expected),
                &candidate.changes,
                candidate.discard_external_changes,
                &validation_token,
                &confirmations,
            )
        })
        .await
        .map_err(|_| ErrorCode::ServiceUnavailable)?
        .map_err(error_code)?;

        match begin {
            BeginApply::Existing(_) => operation_result(&self.store, &actor, &query_operation_id),
            BeginApply::Accepted(permit) => {
                let initial = operation_result(&self.store, &actor, &query_operation_id)?;
                let owner = self.clone();
                tokio::spawn(async move {
                    owner.run_apply(actor, query_operation_id, permit).await;
                });
                Ok(initial)
            }
        }
    }

    pub(super) async fn external_diff(&self) -> Result<super::contract::ExternalDiff, ErrorCode> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || super::config_query::external::external_diff(&store))
            .await
            .map_err(|_| ErrorCode::ServiceUnavailable)?
    }

    /// 文件还原在阻塞 owner 中完成；HTTP 取消不会终止已受理的文件事务。
    pub(super) async fn restore_files(
        &self,
        actor: String,
        request: FileSyncRequest,
    ) -> Result<OperationResult, ErrorCode> {
        let store = Arc::clone(&self.store);
        let operation_id = request.operation_id.as_str().to_owned();
        let task_actor = actor.clone();
        let task_operation_id = operation_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            store.restore_files(
                &task_actor,
                &task_operation_id,
                &expected(&request.expected),
                request.discard_external_changes,
            )
        })
        .await
        .map_err(|_| ErrorCode::ServiceUnavailable)?;
        mutation_result(&self.store, &actor, &operation_id, result.map(|_| ()))
    }

    /// 持久化重试只推进已有 operation，不重新构造或应用 Runtime。
    pub(super) async fn retry_persistence(
        &self,
        actor: String,
        request: FileSyncRequest,
    ) -> Result<OperationResult, ErrorCode> {
        let store = Arc::clone(&self.store);
        let operation_id = request.operation_id.as_str().to_owned();
        let task_actor = actor.clone();
        let task_operation_id = operation_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            store.retry_persistence(
                &task_actor,
                &task_operation_id,
                &expected(&request.expected),
                request.discard_external_changes,
            )
        })
        .await
        .map_err(|_| ErrorCode::ServiceUnavailable)?;
        mutation_result(&self.store, &actor, &operation_id, result.map(|_| ()))
    }

    async fn run_apply(
        self,
        actor: String,
        operation_id: String,
        permit: crate::config::store::active::ApplyPermit,
    ) {
        let prepared = tokio::task::spawn_blocking(move || {
            let resolved = permit
                .resolve_runtime_config()
                .map_err(|_| OperationFailure::ValidationFailed)
                .and_then(|resolved| {
                    resolved
                        .validate_secret_refs(64 * 1024)
                        .map_err(|_| OperationFailure::ApplyFailed)?;
                    Ok(resolved)
                });
            (permit, resolved)
        })
        .await;
        let (mut permit, resolved) = match prepared {
            Ok(value) => value,
            Err(_) => return,
        };
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(failure) => {
                let _ = permit.reject_with(failure, true);
                return;
            }
        };
        let runtime_revision = match self.store.active_snapshot() {
            Ok(snapshot) => RuntimeRevision(snapshot.runtime_revision),
            Err(_) => {
                let _ = permit.reject_with(OperationFailure::ApplyFailed, true);
                return;
            }
        };
        let next_revision = match runtime_revision.0.checked_add(1) {
            Some(revision) => RuntimeRevision(revision),
            None => {
                let _ = permit.reject_with(OperationFailure::ApplyFailed, true);
                return;
            }
        };
        let deadline = Deadline::new(Instant::now() + APPLY_TIMEOUT);
        let prepared = match PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            resolved,
            next_revision,
            deadline,
            Cancellation::new(),
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(_) => {
                let _ = permit.reject_with(OperationFailure::ApplyFailed, true);
                return;
            }
        };
        let stage = tokio::task::spawn_blocking(move || {
            let result = permit.begin_runtime_apply();
            (permit, result)
        })
        .await;
        let (permit, stage_result) = match stage {
            Ok(value) => value,
            Err(_) => return,
        };
        if let Err(error) = stage_result {
            let failure = active_failure(&error);
            let _ = tokio::task::spawn_blocking(move || permit.reject_with(failure, true)).await;
            return;
        }

        let receipt = match self.control.try_apply(runtime_revision, prepared, deadline) {
            Ok(receipt) => receipt,
            Err(error) => {
                let failure = control_failure(&error);
                let _ =
                    tokio::task::spawn_blocking(move || permit.reject_with(failure, true)).await;
                return;
            }
        };
        match receipt.owner_outcome().await {
            Ok(revision) => {
                let store = Arc::clone(&self.store);
                let persisted_operation_id = operation_id.clone();
                let persisted = tokio::task::spawn_blocking(move || {
                    permit.applied(revision.0)?;
                    store.persist_applied(&actor, &persisted_operation_id)
                })
                .await;
                if !matches!(persisted, Ok(Ok(_))) {
                    tracing::warn!(
                        event = "configuration_operation_incomplete",
                        component = "management",
                        operation_id = %operation_id,
                        "configuration_operation_incomplete"
                    );
                }
            }
            Err(error) => {
                if matches!(error, crate::service::ControlError::OutcomeUnknown) {
                    drop(permit);
                    return;
                }
                let compensated = !matches!(
                    error,
                    crate::service::ControlError::Apply(ServiceReloadError::LoggingCompensation(_))
                );
                let failure = control_failure(&error);
                let _ =
                    tokio::task::spawn_blocking(move || permit.reject_with(failure, compensated))
                        .await;
            }
        }
    }
}

pub(super) fn routes() -> Router<Arc<AuthServices>> {
    Router::new()
        .route("/api/v2/config/validate", post(post_validate))
        .route("/api/v2/config/apply", post(post_apply))
        .route(
            "/api/v2/config/modules/{module}/validate",
            post(post_module_validate),
        )
        .route(
            "/api/v2/config/modules/{module}/apply",
            post(post_module_apply),
        )
        .route(
            "/api/v2/config/operations/{operation_id}",
            get(get_operation),
        )
        .route("/api/v2/config/files/diff", get(get_external_diff))
        .route("/api/v2/config/files/restore", post(post_restore))
        .route("/api/v2/config/files/retry", post(post_retry))
        .layer(DefaultBodyLimit::max(super::contract::MAX_MUTATION_BYTES))
}

async fn post_validate(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    Extension(session): Extension<SessionView>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Some(owner) = &services.config_mutations else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let body = match body_bytes(body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let candidate = match decode_candidate(&body) {
        Ok(candidate) => candidate,
        Err(error) => return v2_error_response(error, &request_id),
    };
    v2_result(
        owner.validate(session.user.name, candidate).await,
        &request_id,
    )
}

async fn post_apply(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    Extension(session): Extension<SessionView>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Some(owner) = &services.config_mutations else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let body = match body_bytes(body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let request = match decode_apply(&body, None) {
        Ok(request) => request,
        Err(error) => return v2_error_response(error, &request_id),
    };
    operation_response(
        owner.start_apply(session.user.name, request).await,
        &request_id,
    )
}

/// 单模块入口只复用整体事务编排，不允许借通用封套修改其他模块。
async fn post_module_validate(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    Extension(session): Extension<SessionView>,
    headers: HeaderMap,
    module: Result<Path<String>, axum::extract::rejection::PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let module = match parse_module_path(module) {
        Ok(module) => module,
        Err(error) => return v2_error_response(error, &request_id),
    };
    let Some(owner) = &services.config_mutations else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let body = match body_bytes(body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let candidate = match decode_module_candidate(&body, module) {
        Ok(candidate) => candidate,
        Err(error) => return v2_error_response(error, &request_id),
    };
    v2_result(
        owner.validate(session.user.name, candidate).await,
        &request_id,
    )
}

async fn post_module_apply(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    Extension(session): Extension<SessionView>,
    headers: HeaderMap,
    module: Result<Path<String>, axum::extract::rejection::PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let module = match parse_module_path(module) {
        Ok(module) => module,
        Err(error) => return v2_error_response(error, &request_id),
    };
    let Some(owner) = &services.config_mutations else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let body = match body_bytes(body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let request = match decode_apply(&body, Some(module)) {
        Ok(request) => request,
        Err(error) => return v2_error_response(error, &request_id),
    };
    operation_response(
        owner.start_apply(session.user.name, request).await,
        &request_id,
    )
}

fn parse_module_path(
    module: Result<Path<String>, axum::extract::rejection::PathRejection>,
) -> Result<crate::config::edit::ConfigModule, ErrorCode> {
    let Ok(Path(module)) = module else {
        return Err(ErrorCode::InvalidArgument);
    };
    match module.as_str() {
        "listener" => Ok(crate::config::edit::ConfigModule::Listener),
        "upstreams" => Ok(crate::config::edit::ConfigModule::Upstreams),
        "strategy" => Ok(crate::config::edit::ConfigModule::Strategy),
        "hosts" => Ok(crate::config::edit::ConfigModule::Hosts),
        "outbound" => Ok(crate::config::edit::ConfigModule::Outbound),
        "rule_set" => Ok(crate::config::edit::ConfigModule::RuleSet),
        "clients" => Ok(crate::config::edit::ConfigModule::Clients),
        "dns" => Ok(crate::config::edit::ConfigModule::Dns),
        "statistics" => Ok(crate::config::edit::ConfigModule::Statistics),
        "logs" => Ok(crate::config::edit::ConfigModule::Logs),
        _ => Err(ErrorCode::NotFound),
    }
}

async fn get_operation(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    Extension(session): Extension<SessionView>,
    operation_id: Result<Path<String>, axum::extract::rejection::PathRejection>,
) -> Response {
    let Ok(Path(operation_id)) = operation_id else {
        return v2_error_response(ErrorCode::InvalidArgument, &request_id);
    };
    operation_response(
        operation_result(&services.config_store, &session.user.name, &operation_id),
        &request_id,
    )
}

async fn get_external_diff(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(owner) = &services.config_mutations else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    v2_result(owner.external_diff().await, &request_id)
}

async fn post_restore(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    Extension(session): Extension<SessionView>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Some(owner) = &services.config_mutations else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let body = match body_bytes(body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let request = match decode_file_sync(&body) {
        Ok(request) => request,
        Err(error) => return v2_error_response(error, &request_id),
    };
    operation_response(
        owner.restore_files(session.user.name, request).await,
        &request_id,
    )
}

async fn post_retry(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    Extension(session): Extension<SessionView>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Some(owner) = &services.config_mutations else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let body = match body_bytes(body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let request = match decode_file_sync(&body) {
        Ok(request) => request,
        Err(error) => return v2_error_response(error, &request_id),
    };
    operation_response(
        owner.retry_persistence(session.user.name, request).await,
        &request_id,
    )
}

fn v2_result<T: serde::Serialize>(
    result: Result<T, ErrorCode>,
    request_id: &RequestId,
) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => v2_error_response(error, request_id),
    }
}

fn body_bytes(
    body: Result<Bytes, BytesRejection>,
    request_id: &RequestId,
) -> Result<Bytes, Response> {
    body.map_err(|error| {
        let code = if error.into_response().status() == StatusCode::PAYLOAD_TOO_LARGE {
            ErrorCode::PayloadTooLarge
        } else {
            ErrorCode::InvalidArgument
        };
        v2_error_response(code, request_id)
    })
}

fn operation_response(
    result: Result<OperationResult, ErrorCode>,
    request_id: &RequestId,
) -> Response {
    match result {
        Ok(operation) => {
            let status = if matches!(
                operation.status,
                OperationStatus::Preparing {}
                    | OperationStatus::Applying {}
                    | OperationStatus::Persisting { .. }
            ) {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            };
            (status, Json(operation)).into_response()
        }
        Err(error) => v2_error_response(error, request_id),
    }
}

fn mutation_result(
    store: &ConfigStore,
    actor: &str,
    operation_id: &str,
    result: Result<(), ActiveError>,
) -> Result<OperationResult, ErrorCode> {
    match result {
        Ok(()) => operation_result(store, actor, operation_id),
        Err(error) => {
            let code = error_code(error);
            match operation_result(store, actor, operation_id) {
                Ok(operation) if !matches!(operation.status, OperationStatus::Unknown {}) => {
                    Ok(operation)
                }
                _ => Err(code),
            }
        }
    }
}

fn expected(value: &Preconditions) -> ExpectedRevisions {
    ExpectedRevisions {
        active: value.active_revision.as_str().to_owned(),
        files: value.observed_file_revision.as_str().to_owned(),
    }
}

fn revision(value: String) -> Result<Revision, ErrorCode> {
    Revision::try_from(value).map_err(|_| ErrorCode::ServiceUnavailable)
}

fn expires_at_millis(ttl: Duration) -> Result<u64, ErrorCode> {
    SystemTime::now()
        .checked_add(ttl)
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .ok_or(ErrorCode::ServiceUnavailable)
}

fn impact(value: ImpactKind) -> Impact {
    match value {
        ImpactKind::RenameReferences => Impact::RenameReferences,
        ImpactKind::ListenerRebind => Impact::ListenerRebind,
        ImpactKind::RetentionShortening => Impact::RetentionShortening,
        ImpactKind::DiscardExternalChanges => Impact::DiscardExternalChanges,
    }
}

fn impact_kind(value: Impact) -> ImpactKind {
    match value {
        Impact::RenameReferences => ImpactKind::RenameReferences,
        Impact::ListenerRebind => ImpactKind::ListenerRebind,
        Impact::RetentionShortening => ImpactKind::RetentionShortening,
        Impact::DiscardExternalChanges => ImpactKind::DiscardExternalChanges,
    }
}

fn active_failure(error: &ActiveError) -> OperationFailure {
    match error {
        ActiveError::ActiveConflict => OperationFailure::ActiveRevisionConflict,
        ActiveError::FileConflict | ActiveError::ExternalConfirmation => {
            OperationFailure::FileRevisionConflict
        }
        ActiveError::Candidate(_) | ActiveError::ValidationExpired => {
            OperationFailure::ValidationFailed
        }
        _ => OperationFailure::ApplyFailed,
    }
}

fn control_failure(error: &crate::service::ControlError) -> OperationFailure {
    match error {
        crate::service::ControlError::RevisionConflict { .. }
        | crate::service::ControlError::InvalidCandidateRevision => {
            OperationFailure::ActiveRevisionConflict
        }
        crate::service::ControlError::Apply(ServiceReloadError::LoggingCompensation(_)) => {
            OperationFailure::CompensationFailed
        }
        _ => OperationFailure::ApplyFailed,
    }
}

fn affected_names(changes: &[ConfigChange]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for change in changes {
        match change {
            ConfigChange::Listener(value) => {
                resource_names(value, crate::config::model::ListenerDto::name, &mut names)
            }
            ConfigChange::Upstreams(value) => {
                resource_names(value, crate::config::model::UpstreamDto::name, &mut names)
            }
            ConfigChange::Strategy(value) => resource_names(value, |value| &value.name, &mut names),
            ConfigChange::Hosts(value) => resource_names(
                value,
                crate::config::model::HostsResourceDto::name,
                &mut names,
            ),
            ConfigChange::Outbound(value) => resource_names(value, |value| &value.name, &mut names),
            ConfigChange::RuleSet(value) => {
                resource_names(value, crate::config::model::RuleSetDto::name, &mut names)
            }
            ConfigChange::Clients(crate::config::edit::ClientMutation::Create { value }) => {
                names.insert(value.name.clone());
            }
            ConfigChange::Clients(crate::config::edit::ClientMutation::Update {
                original_name,
                value,
            }) => {
                names.insert(original_name.clone());
                names.insert(value.name.clone());
            }
            ConfigChange::Dns(_) | ConfigChange::Statistics(_) | ConfigChange::Logs(_) => {}
        }
    }
    names.into_iter().collect()
}

fn resource_names<T>(
    mutation: &ResourceMutation<T>,
    name: impl Fn(&T) -> &str,
    names: &mut BTreeSet<String>,
) {
    match mutation {
        ResourceMutation::Create { value } => {
            names.insert(name(value).to_owned());
        }
        ResourceMutation::Update {
            original_name,
            value,
        } => {
            names.insert(original_name.clone());
            names.insert(name(value).to_owned());
        }
    }
}
