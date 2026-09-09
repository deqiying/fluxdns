//! Management API 的只读查询、参数校验与安全响应投影。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::rejection::PathRejection;
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use time::Date;

use super::config_query;
use super::contract::{
    ConfigModule, ConfigRead, ConfigState, DecimalU64, ErrorCode, ProcessMetrics, RetentionPreview,
    RetentionPreviewRequest, RetentionStatus as RetentionStatusResponse, ServiceMetrics,
    SystemConfigRead,
};
use super::metrics::MetricsOwner;
use super::router::{AuthServices, RequestId, internal_error, v2_error_response};
use crate::dns::Deadline;
use crate::runtime::RuntimeCoordinator;
use crate::storage::{
    RetentionCoordinator, RetentionPlan, RetentionPolicy, next_scheduled_at_utc_millis,
};

mod history;

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const UNIX_EPOCH_JULIAN_DAY: i32 = 2_440_588;

pub(crate) struct ManagementQueryService {
    coordinator: Arc<RuntimeCoordinator>,
    metrics: Arc<MetricsOwner>,
    retention: Arc<RetentionCoordinator>,
    detail_store: Arc<crate::storage::DetailShardStore>,
}

/// 历史查询共享同一保留水位 owner 与日分片读口，避免两者被独立接线。
pub(crate) struct ManagementHistoryDependencies {
    pub(super) retention: Arc<RetentionCoordinator>,
    pub(super) detail_store: Arc<crate::storage::DetailShardStore>,
}

impl ManagementHistoryDependencies {
    pub(crate) fn new(
        retention: Arc<RetentionCoordinator>,
        detail_store: Arc<crate::storage::DetailShardStore>,
    ) -> Self {
        Self {
            retention,
            detail_store,
        }
    }
}

impl ManagementQueryService {
    pub(crate) fn new(
        coordinator: Arc<RuntimeCoordinator>,
        metrics: Arc<MetricsOwner>,
        history: ManagementHistoryDependencies,
    ) -> Self {
        Self {
            coordinator,
            metrics,
            retention: history.retention,
            detail_store: history.detail_store,
        }
    }

    fn service_metrics(&self) -> ServiceMetrics {
        self.metrics.service_metrics()
    }

    fn process_metrics(&self) -> ProcessMetrics {
        self.metrics.process_metrics()
    }

    pub(super) fn detail_store(&self) -> Arc<crate::storage::DetailShardStore> {
        Arc::clone(&self.detail_store)
    }

    pub(super) fn project_committed_records(
        &self,
        store: &crate::config::store::ConfigStore,
        filter: super::contract::QueryFilter,
        records: &[crate::storage::DetailCommittedRecord],
    ) -> Result<(super::contract::Revision, Vec<super::contract::QueryRecord>), ErrorCode> {
        history::project_committed_records(self, store, filter, records)
    }

    fn config_state(
        &self,
        store: &crate::config::store::ConfigStore,
    ) -> Result<ConfigState, ErrorCode> {
        config_query::configuration_state(store)
    }

    fn config_module(
        &self,
        store: &crate::config::store::ConfigStore,
        module: ConfigModule,
    ) -> Result<ConfigRead, ErrorCode> {
        config_query::configuration_module(store, &self.coordinator, module)
    }

    fn system_config(
        &self,
        store: &crate::config::store::ConfigStore,
    ) -> Result<SystemConfigRead, ErrorCode> {
        config_query::system_configuration(store, &self.coordinator)
    }

    async fn retention(
        &self,
        store: &crate::config::store::ConfigStore,
    ) -> Result<RetentionStatusResponse, ErrorCode> {
        let active = store
            .active_snapshot()
            .map_err(|_| ErrorCode::ServiceUnavailable)?;
        if active.runtime_revision != self.coordinator.load().revision().0 {
            return Err(ErrorCode::ServiceUnavailable);
        }
        let policy_source = active.config.statistics.clone();
        let policy = RetentionPolicy::new(
            policy_source.retention.days,
            policy_source.retention.grace_days,
            policy_source.retention.reference_size_bytes,
        )
        .map_err(|_| ErrorCode::ServiceUnavailable)?;
        let sampled_at = SystemTime::now();
        let reference_day =
            crate::storage::day_utc(sampled_at).map_err(|_| ErrorCode::ServiceUnavailable)?;
        let status = self
            .retention
            .status(policy, reference_day, query_deadline())
            .await
            .map_err(|_| ErrorCode::ServiceUnavailable)?;
        Ok(RetentionStatusResponse {
            policy: policy_source,
            sampled_at_ms: unix_millis_u64(sampled_at).ok_or(ErrorCode::ServiceUnavailable)?,
            detail_bytes: DecimalU64::from(status.sampled_detail_bytes),
            cutoff_utc_date: status
                .published
                .map(|state| format_epoch_day(state.retired_before_day_utc)),
            last_completed_at_ms: status.last_cleanup_at.and_then(unix_millis_u64),
            next_scheduled_at_ms: next_scheduled_at_utc_millis(sampled_at).ok(),
            pending_reclaim_bytes: DecimalU64::from(status.pending_reclaim_bytes),
        })
    }

    /// 预览使用真实详情文件采样，但不发布水位、不创建回收任务。
    async fn retention_preview(
        &self,
        store: &crate::config::store::ConfigStore,
        request: RetentionPreviewRequest,
    ) -> Result<RetentionPreview, ErrorCode> {
        let state = config_query::configuration_state(store)?;
        if request.expected.active_revision.as_str() != state.active_revision.as_str() {
            return Err(ErrorCode::ActiveRevisionConflict);
        }
        if request.expected.observed_file_revision.as_str() != state.observed_file_revision.as_str()
        {
            return Err(ErrorCode::FileRevisionConflict);
        }
        let active = store
            .active_snapshot()
            .map_err(|_| ErrorCode::ServiceUnavailable)?;
        if active.runtime_revision != self.coordinator.load().revision().0 {
            return Err(ErrorCode::ServiceUnavailable);
        }
        let proposed_policy = RetentionPolicy::new(
            request.policy.retention.days,
            request.policy.retention.grace_days,
            request.policy.retention.reference_size_bytes,
        )
        .map_err(|_| ErrorCode::InvalidArgument)?;
        let current_policy = RetentionPolicy::new(
            active.config.statistics.retention.days,
            active.config.statistics.retention.grace_days,
            active.config.statistics.retention.reference_size_bytes,
        )
        .map_err(|_| ErrorCode::ServiceUnavailable)?;
        let sampled_at = SystemTime::now();
        let reference_day =
            crate::storage::day_utc(sampled_at).map_err(|_| ErrorCode::ServiceUnavailable)?;
        let proposed = self
            .retention
            .preview(proposed_policy, reference_day, query_deadline())
            .await
            .map_err(|_| ErrorCode::ServiceUnavailable)?;
        let current =
            RetentionPlan::calculate(current_policy, reference_day, proposed.sampled_detail_bytes)
                .map_err(|_| ErrorCode::ServiceUnavailable)?;
        Ok(RetentionPreview {
            expected: request.expected,
            sampled_at_ms: unix_millis_u64(sampled_at).ok_or(ErrorCode::ServiceUnavailable)?,
            detail_bytes: DecimalU64::from(proposed.sampled_detail_bytes),
            proposed_cutoff_utc_date: format_epoch_day(proposed.keep_from_day_utc),
            shortens_history: proposed.keep_from_day_utc > current.keep_from_day_utc,
        })
    }
}

pub(crate) fn routes() -> Router<Arc<AuthServices>> {
    Router::new()
        .route("/api/v2/service/metrics", get(get_service_metrics))
        .route("/api/v2/system/runtime", get(get_process_metrics))
        .route("/api/v2/config/state", get(get_config_state))
        .route("/api/v2/config/system", get(get_system_config))
        .route("/api/v2/config/modules/{module}", get(get_config_module))
        .route("/api/v2/retention", get(get_retention))
        .route("/api/v2/retention/preview", post(post_retention_preview))
        .route("/api/v2/queries/search", post(history::post_query_search))
        .route(
            "/api/v2/queries/{record_id}",
            get(history::get_query_detail),
        )
}

async fn get_service_metrics(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    Json(queries.service_metrics()).into_response()
}

async fn get_process_metrics(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    Json(queries.process_metrics()).into_response()
}

async fn get_config_state(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    v2_result(queries.config_state(&services.config_store), &request_id)
}

async fn get_system_config(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    v2_result(queries.system_config(&services.config_store), &request_id)
}

async fn get_config_module(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    module: Result<Path<String>, PathRejection>,
) -> Response {
    let Some(queries) = &services.queries else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let Ok(Path(module)) = module else {
        return v2_error_response(ErrorCode::InvalidArgument, &request_id);
    };
    let Some(module) = parse_config_module(&module) else {
        return v2_error_response(ErrorCode::NotFound, &request_id);
    };
    v2_result(
        queries.config_module(&services.config_store, module),
        &request_id,
    )
}

async fn get_retention(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    v2_result(queries.retention(&services.config_store).await, &request_id)
}

async fn post_retention_preview(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    body: Result<Json<RetentionPreviewRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return v2_error_response(ErrorCode::InvalidArgument, &request_id);
    };
    let Some(queries) = &services.queries else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    v2_result(
        queries
            .retention_preview(&services.config_store, request)
            .await,
        &request_id,
    )
}

fn v2_result<T: Serialize>(result: Result<T, ErrorCode>, request_id: &RequestId) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(code) => v2_error_response(code, request_id),
    }
}

fn parse_config_module(value: &str) -> Option<ConfigModule> {
    match value {
        "listener" => Some(ConfigModule::Listener),
        "upstreams" => Some(ConfigModule::Upstreams),
        "strategy" => Some(ConfigModule::Strategy),
        "hosts" => Some(ConfigModule::Hosts),
        "outbound" => Some(ConfigModule::Outbound),
        "rule_set" => Some(ConfigModule::RuleSet),
        "clients" => Some(ConfigModule::Clients),
        "dns" => Some(ConfigModule::Dns),
        "statistics" => Some(ConfigModule::Statistics),
        "logs" => Some(ConfigModule::Logs),
        _ => None,
    }
}

fn format_epoch_day(day: i32) -> String {
    day.checked_add(UNIX_EPOCH_JULIAN_DAY)
        .and_then(|value| Date::from_julian_day(value).ok())
        .map_or_else(
            || "1970-01-01".to_owned(),
            |date| {
                format!(
                    "{:04}-{:02}-{:02}",
                    date.year(),
                    u8::from(date.month()),
                    date.day()
                )
            },
        )
}

fn query_deadline() -> Deadline {
    Deadline::new(Instant::now() + QUERY_TIMEOUT)
}

fn unix_millis_u64(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use axum::body::{Body, to_bytes};
    use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ORIGIN};
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;
    use crate::config::edit::ConfigChange;
    use crate::config::model::{LogLevelDto, LogsDto};
    use crate::config::store::ConfigStore;
    use crate::config::store::active::BeginApply;
    use crate::config::{ConfigV2Loader, LoadOptions};
    use crate::dns::TransportClass;
    use crate::dns::{Cancellation, RuntimeRevision};
    use crate::management::auth::AuthState;
    use crate::management::router::{AuthServices, build_router};
    use crate::management::session::SessionStore;
    use crate::ports::effects::{
        ActivatedSocket, ActivatedSocketHandle, PreparedSocket, SocketFactory, SocketKind,
        SocketSpec,
    };
    use crate::ports::observation::ClientMatchSource;
    use crate::ports::storage::{ResolveAnswer, ResolveEvent, StatsSource};
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::ports::{PortError, PortErrorClass, PortFuture};
    use crate::runtime::{PreparedRuntime, RuntimeCoordinator, bind_prepared};

    pub(crate) const HISTORY_DAY: u64 = 20_710;

    #[tokio::test]
    async fn retired_v1_routes_are_json_404_and_auth_errors_use_v2_envelope() {
        let (services, _) = test_services().await;
        let app = build_router(services);
        for path in [
            "auth/setup",
            "auth/login",
            "auth/session",
            "auth/refresh",
            "auth/logout",
            "overview",
            "runtime",
            "health",
            "statistics",
            "queries",
            "resources",
            "system",
        ] {
            for method in ["GET", "POST"] {
                let request = Request::builder()
                    .method(method)
                    .uri(format!("/api/v1/{path}"))
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap();
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
                assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
                let body = to_bytes(response.into_body(), 4096).await.unwrap();
                let error: crate::management::contract::ErrorEnvelope =
                    serde_json::from_slice(&body).unwrap();
                assert!(matches!(error.code, ErrorCode::NotFound));
                assert!(error.field_errors.is_empty());
            }
        }
        let mut request = post_json("/api/v2/auth/login", None, "{}".to_owned());
        request
            .headers_mut()
            .insert(ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:45678".parse::<SocketAddr>().unwrap(),
        ));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let error: crate::management::contract::ErrorEnvelope =
            serde_json::from_slice(&body).unwrap();
        assert!(matches!(error.code, ErrorCode::InvalidArgument));
    }

    pub(crate) fn history_record(
        day: u64,
        offset: u64,
        source: StatsSource,
    ) -> crate::storage::ResolveDetailRecord {
        crate::storage::ResolveDetailRecord::from_event(ResolveEvent {
            occurred_at: UNIX_EPOCH + Duration::from_millis(day * 86_400_000 + offset),
            duration_millis: if source == StatsSource::Cache { 4 } else { 7 },
            dns_core_duration_micros: 900,
            request_digest: Arc::from("management-history-test"),
            listener_id: Arc::from("local"),
            route_id: None,
            client_id: Some(Arc::from("raw-device")),
            client_ip: Some("192.0.2.10".parse().unwrap()),
            client_match_source: Some(ClientMatchSource::Ip),
            matched_client_id: Some(Arc::from("Desktop-01")),
            client_bucket: Some(Arc::from("Desktop-01")),
            strategy_id: Some(Arc::from("default")),
            upstream_id: Some(Arc::from("local")),
            upstream_member_id: None,
            upstream_used_id: Some(Arc::from("local")),
            matched_rule_source: None,
            matched_resource_id: None,
            matched_rule_ordinal: None,
            resource_version: None,
            transport: TransportClass::Datagram,
            qname: Arc::from(if source == StatsSource::Cache {
                "cached.example."
            } else {
                "direct.example."
            }),
            qtype: 1,
            qclass: 1,
            answers: vec![ResolveAnswer {
                name: "answer.example.".to_owned(),
                record_type: "A".to_owned(),
                data: "192.0.2.20".to_owned(),
                ttl: 30,
            }],
            rcode: 0,
            cancellation_reason: None,
            outcome: OutcomeClass::Success,
            source,
            cache_status: if source == StatsSource::Cache {
                CacheStatus::Stale
            } else {
                CacheStatus::Miss
            },
            runtime_revision: RuntimeRevision(7),
        })
        .unwrap()
    }

    struct FakeSocketFactory;

    struct FakePreparedSocket(SocketSpec);
    struct FakeActivatedSocket(SocketSpec);

    impl SocketFactory for FakeSocketFactory {
        fn prepare<'a>(
            &'a self,
            spec: SocketSpec,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Box<dyn PreparedSocket>, PortError>> {
            Box::pin(
                async move { Ok(Box::new(FakePreparedSocket(spec)) as Box<dyn PreparedSocket>) },
            )
        }
    }

    impl PreparedSocket for FakePreparedSocket {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(self.0.address)
        }

        fn activate(self: Box<Self>) -> Result<Box<dyn ActivatedSocket>, PortError> {
            Ok(Box::new(FakeActivatedSocket(self.0)))
        }
    }

    impl ActivatedSocket for FakeActivatedSocket {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(self.0.address)
        }

        fn kind(&self) -> SocketKind {
            self.0.kind
        }

        fn socket_handle(&self) -> Result<ActivatedSocketHandle, PortError> {
            Err(PortError::new(
                PortErrorClass::Unavailable,
                "management_query_test.socket_handle",
            ))
        }
    }

    pub(crate) async fn test_services() -> (Arc<AuthServices>, PathBuf) {
        let root = PathBuf::from(crate::config::test_support::absolute_path(
            "management-query-router-v2",
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source_path = root.join("config.yaml");
        let source = include_str!("../../tests/fixtures/config-v2.yaml");
        std::fs::write(&source_path, source.as_bytes()).unwrap();
        let output = ConfigV2Loader::new(LoadOptions::default().without_snapshot())
            .load_from_path(&source_path)
            .unwrap();
        let storage_runtime = crate::storage::StorageRuntime::open(
            &output.resolved,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .unwrap();
        let retention = storage_runtime.retention_coordinator();
        let detail_store = storage_runtime.detail_store();
        detail_store
            .write_records(
                i32::try_from(HISTORY_DAY).unwrap(),
                &[history_record(HISTORY_DAY, 1, StatsSource::Upstream)],
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        detail_store
            .write_records(
                i32::try_from(HISTORY_DAY + 1).unwrap(),
                &[history_record(HISTORY_DAY + 1, 1, StatsSource::Cache)],
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        let prepared = PreparedRuntime::prepare(output.resolved, RuntimeRevision(7)).unwrap();
        let candidate = bind_prepared(
            prepared,
            &FakeSocketFactory,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let metrics = Arc::new(MetricsOwner::new());
        metrics.set_process_sample_for_test(64 * 1024 * 1024, 1.25, 8);
        let query_service = Arc::new(ManagementQueryService::new(
            Arc::new(RuntimeCoordinator::new(candidate)),
            metrics,
            ManagementHistoryDependencies::new(retention, detail_store),
        ));
        let auth = Arc::new(AuthState::new(&[]).unwrap());
        let sessions = Arc::new(SessionStore::new(false));
        let store = Arc::new(
            ConfigStore::with_active_source(source_path, source, RuntimeRevision(7).0).unwrap(),
        );
        (
            Arc::new(AuthServices::new(
                auth,
                sessions,
                store,
                "http://127.0.0.1:8080".to_owned(),
                Some(query_service),
            )),
            root,
        )
    }

    fn get(path: &str, authorization: Option<&str>) -> Request<Body> {
        let mut request = Request::builder().uri(path);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        request.body(Body::empty()).unwrap()
    }

    fn post_json(path: &str, authorization: Option<&str>, body: String) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(CONTENT_TYPE, "application/json");
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        request.body(Body::from(body)).unwrap()
    }

    #[tokio::test]
    async fn authenticated_router_serves_all_read_only_contracts() {
        let (services, root) = test_services().await;
        let issued = services.sessions.issue("admin".to_owned()).unwrap();
        let authorization = format!("Bearer {}", issued.view.access_token);
        let app = build_router(Arc::clone(&services));
        let paths = [
            "/api/v2/service/metrics",
            "/api/v2/system/runtime",
            "/api/v2/config/state",
            "/api/v2/config/system",
            "/api/v2/config/modules/listener",
            "/api/v2/config/modules/upstreams",
            "/api/v2/config/modules/strategy",
            "/api/v2/config/modules/hosts",
            "/api/v2/config/modules/outbound",
            "/api/v2/config/modules/rule_set",
            "/api/v2/config/modules/clients",
            "/api/v2/config/modules/dns",
            "/api/v2/config/modules/statistics",
            "/api/v2/config/modules/logs",
            "/api/v2/retention",
        ];
        let mut service_rss = None;
        for path in paths {
            let response = app
                .clone()
                .oneshot(get(path, Some(&authorization)))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers().get(CONTENT_TYPE).unwrap(),
                "application/json",
                "{path}"
            );
            assert!(response.headers().contains_key("x-request-id"), "{path}");
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(body.is_object(), "{path}");
            if path == "/api/v2/service/metrics" {
                assert_eq!(body["rss_bytes"]["state"], "available");
                assert_eq!(body["qps"]["reason"], "warmup");
                assert!(body["qps"]["observed_seconds"].is_u64());
                service_rss = Some(body["rss_bytes"].clone());
            }
            if path == "/api/v2/system/runtime" {
                assert_eq!(body["cpu_percent"]["value"], 1.25);
                assert_eq!(body["threads"]["value"], 8);
                assert_eq!(Some(body["rss_bytes"].clone()), service_rss);
            }
            if path == "/api/v2/config/state" {
                assert_eq!(body["runtime_revision"], "7");
                assert_eq!(body["synchronization"], "synced");
            }
            if path == "/api/v2/config/system" {
                assert_eq!(body["work_path"], ".");
                assert_eq!(body["records_path"], "./data/queries");
                assert!(body.get("users").is_none());
                assert!(body.get("input_hash").is_none());
            }
            if let Some(module) = path.strip_prefix("/api/v2/config/modules/") {
                let values = body["values"].as_array().unwrap();
                assert!(
                    values.iter().all(|value| value["module"] == module),
                    "{path} returned another module"
                );
                assert!(body["effective"].is_array());
                assert!(body["references"].is_array());
                assert!(body["runtime"].is_array());
                match module {
                    "listener" => {
                        assert_eq!(values.len(), 1);
                        assert_eq!(body["runtime"][0]["bindings"][0]["port"], 15353);
                        assert_eq!(body["references"][0]["to_name"], "default");
                    }
                    "upstreams" => assert_eq!(values.len(), 1),
                    "strategy" => {
                        assert_eq!(values.len(), 1);
                        assert_eq!(body["references"].as_array().unwrap().len(), 2);
                    }
                    "hosts" => {
                        assert_eq!(values.len(), 1);
                        assert_eq!(body["runtime"][0]["condition"], "ready");
                    }
                    "clients" => {
                        assert_eq!(values.len(), 1);
                        assert_eq!(body["effective"][0]["source"], "global");
                    }
                    "dns" => {
                        assert_eq!(values.len(), 1);
                        assert_eq!(body["runtime"][0]["snapshot"]["state"], "disabled");
                        assert_eq!(body["runtime"][0]["snapshot"]["owner_revision"], "7");
                    }
                    "statistics" => {
                        assert_eq!(values.len(), 1);
                        assert_eq!(body["effective"].as_array().unwrap().len(), 3);
                    }
                    "logs" => assert_eq!(values.len(), 1),
                    "outbound" | "rule_set" => assert!(values.len() <= 1),
                    _ => unreachable!(),
                }
            }
            if path == "/api/v2/retention" {
                assert_eq!(body["policy"]["retention"]["days"], 7);
                assert!(body["detail_bytes"].is_string());
                assert!(body["pending_reclaim_bytes"].is_string());
                assert!(body["next_scheduled_at_ms"].is_u64());
            }
            let serialized = serde_json::to_string(&body).unwrap();
            for forbidden in [
                "canonical_qname",
                "client_bucket",
                "request_digest",
                "route_id",
                "password_hash",
                "secret_ref",
            ] {
                assert!(!serialized.contains(forbidden), "{path}: {forbidden}");
            }
            for query_only in ["qname", "client_ip", "answers"] {
                assert!(!serialized.contains(query_only), "{path}: {query_only}");
            }
        }

        let mut search = json!({
            "filter": {
                "from_ms": HISTORY_DAY * 86_400_000,
                "to_ms": (HISTORY_DAY + 2) * 86_400_000
            },
            "cursor": null,
            "direction": "older",
            "page_size": 1,
            "sort": "occurred_at",
            "order": "asc"
        });
        let response = app
            .clone()
            .oneshot(post_json(
                "/api/v2/queries/search",
                Some(&authorization),
                search.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let first: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 1);
        assert_eq!(first["items"][0]["qname"], "direct.example.");
        assert_eq!(first["items"][0]["matched"]["source"], "ip");
        assert_eq!(first["items"][0]["current_client_name"], "desktop");
        assert_eq!(first["items"][0]["duration_us"], 7000);
        assert_eq!(first["items"][0]["cache_producer"], serde_json::Value::Null);
        assert_eq!(first["items"][0]["upstream_target_name"], "local");
        assert!(first["next_cursor"].is_string());
        assert!(first["snapshot_cursor"]["sequence"].is_string());
        assert!(first["retention_revision"].is_string());
        assert!(first["available_from_ms"].is_u64());

        search["cursor"] = first["next_cursor"].clone();
        let response = app
            .clone()
            .oneshot(post_json(
                "/api/v2/queries/search",
                Some(&authorization),
                search.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let second: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(second["items"][0]["qname"], "cached.example.");
        assert_eq!(second["items"][0]["source"], "cache");
        assert_eq!(
            second["items"][0]["upstream_target_name"],
            serde_json::Value::Null
        );
        assert_eq!(
            second["items"][0]["cache_producer"]["upstream_target_name"],
            "local"
        );

        let record_id = first["items"][0]["id"].as_str().unwrap();
        let response = app
            .clone()
            .oneshot(get(
                &format!("/api/v2/queries/{record_id}"),
                Some(&authorization),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail["record"]["id"], record_id);
        assert_eq!(detail["directory_revision"], first["directory_revision"]);

        search["cursor"] = serde_json::Value::Null;
        search["page_size"] = json!(20);
        search["filter"]["client_name"] = json!("missing-name");
        let response = app
            .clone()
            .oneshot(post_json(
                "/api/v2/queries/search",
                Some(&authorization),
                search.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let no_match: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(no_match["items"].as_array().unwrap().is_empty());

        search["filter"]
            .as_object_mut()
            .unwrap()
            .remove("client_name");
        search["filter"]["qname"] = json!("direct.example");
        let response = app
            .clone()
            .oneshot(post_json(
                "/api/v2/queries/search",
                Some(&authorization),
                search.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let canonical: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(canonical["items"].as_array().unwrap().len(), 1);

        search["filter"].as_object_mut().unwrap().remove("qname");
        search["cursor"] = json!("invalid-cursor");
        let response = app
            .clone()
            .oneshot(post_json(
                "/api/v2/queries/search",
                Some(&authorization),
                search.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GONE);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let cursor_error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(cursor_error["code"], "CURSOR_EXPIRED");
        assert_eq!(cursor_error["field_errors"], json!([]));

        search["cursor"] = serde_json::Value::Null;
        search["filter"]["qtype"] = json!("BOGUS");
        let response = app
            .clone()
            .oneshot(post_json(
                "/api/v2/queries/search",
                Some(&authorization),
                search.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = app
            .clone()
            .oneshot(post_json(
                "/api/v2/queries/search",
                None,
                search.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let unauthorized_v2 = app
            .clone()
            .oneshot(get("/api/v2/config/state", None))
            .await
            .unwrap();
        assert_eq!(unauthorized_v2.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(unauthorized_v2.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "AUTH_REQUIRED");
        assert_eq!(body["field_errors"], json!([]));

        let unknown_module = app
            .clone()
            .oneshot(get("/api/v2/config/modules/work", Some(&authorization)))
            .await
            .unwrap();
        assert_eq!(unknown_module.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(unknown_module.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "NOT_FOUND");
        assert_eq!(body["field_errors"], json!([]));

        for path in [
            "/api/v2/config/modules/logs/validate",
            "/api/v2/config/modules/logs/apply",
        ] {
            let request = Request::builder()
                .method("POST")
                .uri(path)
                .header(AUTHORIZATION, &authorization)
                .body(Body::from("{}"))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
        let state_response = app
            .clone()
            .oneshot(get("/api/v2/config/state", Some(&authorization)))
            .await
            .unwrap();
        let state = to_bytes(state_response.into_body(), 4096).await.unwrap();
        let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
        let preview = json!({
            "expected": {
                "active_revision": state["active_revision"],
                "observed_file_revision": state["observed_file_revision"]
            },
            "policy": {"retention": {"days": 1, "grace_days": 0, "reference_size_bytes": 1}}
        });
        let response = app
            .clone()
            .oneshot(post_json(
                "/api/v2/retention/preview",
                Some(&authorization),
                preview.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["expected"], preview["expected"]);
        assert!(body["detail_bytes"].is_string());
        assert!(body["proposed_cutoff_utc_date"].is_string());
        assert!(body["shortens_history"].is_boolean());

        let rejected_origin = Request::builder()
            .method("POST")
            .uri("/api/v2/config/validate")
            .header(AUTHORIZATION, &authorization)
            .header(ORIGIN, "http://example.invalid")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = app.clone().oneshot(rejected_origin).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["code"],
            "FORBIDDEN"
        );

        let unavailable_owner = Request::builder()
            .method("POST")
            .uri("/api/v2/config/validate")
            .header(AUTHORIZATION, &authorization)
            .header(ORIGIN, "http://127.0.0.1:8080")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = app.clone().oneshot(unavailable_owner).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let oversized = Request::builder()
            .method("POST")
            .uri("/api/v2/config/validate")
            .header(AUTHORIZATION, &authorization)
            .header(ORIGIN, "http://127.0.0.1:8080")
            .header(CONTENT_TYPE, "application/json")
            .header(
                CONTENT_LENGTH,
                (crate::management::contract::MAX_MUTATION_BYTES + 1).to_string(),
            )
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(oversized).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["code"],
            "PAYLOAD_TOO_LARGE"
        );

        let expected = services.config_store.observe_files().unwrap().expected();
        let changes = vec![ConfigChange::Logs(LogsDto {
            enable: true,
            level: LogLevelDto::Warn,
            path: "./logs/changed.log".into(),
        })];
        let validated = services
            .config_store
            .validate_edit("test-actor", &expected, &changes, false)
            .unwrap();
        let BeginApply::Accepted(mut permit) = services
            .config_store
            .begin_apply(
                "test-actor",
                "revision-race",
                &expected,
                &changes,
                false,
                &validated.token,
                &validated.impacts,
            )
            .unwrap()
        else {
            panic!("expected a new operation");
        };
        permit.begin_runtime_apply().unwrap();
        permit.applied(8).unwrap();
        let mismatched = app
            .oneshot(get("/api/v2/config/modules/logs", Some(&authorization)))
            .await
            .unwrap();
        assert_eq!(mismatched.status(), StatusCode::SERVICE_UNAVAILABLE);

        let _ = std::fs::remove_dir_all(root);
    }
}
