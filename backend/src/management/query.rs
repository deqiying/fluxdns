//! Management API 的只读查询、参数校验与安全响应投影。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::rejection::PathRejection;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};

use super::config_query;
use super::contract::{
    ConfigModule, ConfigRead, ConfigState, DecimalU64, ErrorCode, ProcessMetrics,
    RetentionStatus as RetentionStatusResponse, ServiceMetrics, SystemConfigRead,
};
use super::metrics::MetricsOwner;
use super::router::{AuthServices, RequestId, internal_error, invalid_argument, v2_error_response};
use crate::config::BindTransport;
use crate::dns::Deadline;
use crate::observability::TelemetryWriter;
use crate::ports::management::{
    ManagementStorageRead, PageRequest, QueryCacheOutcome, QueryDetailStatus, QueryOutcome,
    QueryRcode, QuerySort, QuerySource, QueryTransport, ResolveQuery, ResolveQueryRecord,
    SortOrder, StatisticDimension, StatisticsQuery,
};
use crate::ports::telemetry::{Component as TelemetryComponent, ComponentHealthState};
use crate::resolution::{ResolutionPipelineMetrics, ResolutionPipelineSnapshot};
use crate::resource::{ResourceSourceKind, ResourceStaleStatus};
use crate::runtime::RuntimeCoordinator;
use crate::storage::{RetentionCoordinator, RetentionPolicy, next_scheduled_at_utc_millis};

mod history;

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_PAGE: u32 = 1;
const DEFAULT_PAGE_SIZE: u32 = 20;
const MAX_PAGE_SIZE: u32 = 100;
const MAX_STATISTIC_DAYS: i32 = 31;
const UNIX_EPOCH_JULIAN_DAY: i32 = 2_440_588;

pub(crate) struct ManagementQueryService {
    coordinator: Arc<RuntimeCoordinator>,
    storage: Arc<dyn ManagementStorageRead>,
    telemetry: Option<Arc<TelemetryWriter>>,
    started_at: SystemTime,
    started_instant: Instant,
    resolve_log_enabled: bool,
    resolution_metrics: Arc<ResolutionPipelineMetrics>,
    metrics: Arc<MetricsOwner>,
    retention: Arc<RetentionCoordinator>,
    detail_store: Arc<crate::storage::DetailShardStore>,
}

/// 历史查询共享同一保留水位 owner 与日分片读口，避免两者被独立接线。
pub(crate) struct ManagementHistoryDependencies {
    retention: Arc<RetentionCoordinator>,
    detail_store: Arc<crate::storage::DetailShardStore>,
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
        storage: Arc<dyn ManagementStorageRead>,
        telemetry: Option<Arc<TelemetryWriter>>,
        resolve_log_enabled: bool,
        resolution_metrics: Arc<ResolutionPipelineMetrics>,
        metrics: Arc<MetricsOwner>,
        history: ManagementHistoryDependencies,
    ) -> Self {
        Self {
            coordinator,
            storage,
            telemetry,
            started_at: SystemTime::now(),
            started_instant: Instant::now(),
            resolve_log_enabled,
            resolution_metrics,
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

    async fn overview(&self) -> Result<Overview, QueryError> {
        let runtime = self.coordinator.load();
        let summary = runtime.snapshot().summary();
        let health = self.health();
        let mut cards = Vec::with_capacity(5);
        if self.resolve_log_enabled {
            let since = unix_millis(SystemTime::now()).saturating_sub(86_400_000);
            let counters = self
                .storage
                .overview(since, query_deadline())
                .await
                .map_err(|_| QueryError::Internal)?;
            cards.push(OverviewCard::available(
                "queries_24h",
                "24 小时查询",
                counters.queries as f64,
                "count",
            ));
            cards.push(if counters.queries == 0 {
                OverviewCard::unavailable("cache_hit_rate", "缓存命中率", "NO_QUERY_DATA")
            } else {
                OverviewCard::available(
                    "cache_hit_rate",
                    "缓存命中率",
                    counters.cache_hits as f64 * 100.0 / counters.queries as f64,
                    "percent",
                )
            });
            cards.push(OverviewCard::available(
                "failed_queries_24h",
                "24 小时失败查询",
                counters.failed as f64,
                "count",
            ));
        } else {
            for (key, label) in [
                ("queries_24h", "24 小时查询"),
                ("cache_hit_rate", "缓存命中率"),
                ("failed_queries_24h", "24 小时失败查询"),
            ] {
                cards.push(OverviewCard::unavailable(
                    key,
                    label,
                    "RESOLVE_LOG_DISABLED",
                ));
            }
        }
        cards.push(OverviewCard::available(
            "active_listeners",
            "活动监听",
            runtime.listeners().len() as f64,
            "count",
        ));
        cards.push(OverviewCard::available(
            "resources",
            "资源",
            summary.resource_count as f64,
            "count",
        ));
        Ok(Overview {
            sampled_at: now_rfc3339(),
            runtime_revision: summary.revision.0.to_string(),
            overall_status: health.overall_status,
            cards,
            resolution_pipeline: self.resolution_metrics.snapshot().into(),
        })
    }

    fn runtime(&self) -> RuntimeSnapshot {
        let runtime = self.coordinator.load();
        let snapshot = runtime.snapshot();
        let summary = snapshot.summary();
        let binds = snapshot
            .config()
            .bind_plan
            .entries
            .iter()
            .map(|entry| BindEntry {
                transport: bind_transport_name(entry.transport),
                address: entry.address.to_string(),
                port: entry.port,
                owner: entry.owner.clone(),
                v6_only: entry.v6_only,
                state: if runtime.is_draining() {
                    "draining"
                } else {
                    "active"
                },
            })
            .collect();
        RuntimeSnapshot {
            sampled_at: now_rfc3339(),
            revision: summary.revision.0.to_string(),
            normalized_hash: summary.normalized_hash.chars().take(12).collect(),
            listener_count: summary.listener_count,
            bind_count: summary.bind_entry_count,
            resource_count: summary.resource_count,
            has_policy_core: summary.has_policy_core,
            binds,
        }
    }

    fn health(&self) -> HealthSnapshot {
        let now_instant = Instant::now();
        let now_system = SystemTime::now();
        let mut components = BTreeMap::new();
        if let Some(telemetry) = &self.telemetry {
            for snapshot in telemetry.health_snapshot() {
                components.insert(
                    snapshot.component,
                    ComponentHealth {
                        component: component_name(snapshot.component),
                        status: health_status(snapshot.state),
                        reason_code: health_reason_code(snapshot.state, snapshot.safe_reason),
                        first_changed_at: Some(format_instant(
                            snapshot.first_seen,
                            now_instant,
                            now_system,
                        )),
                        last_changed_at: format_instant(
                            snapshot.last_changed,
                            now_instant,
                            now_system,
                        ),
                        last_success_at: snapshot
                            .last_success
                            .map(|value| format_instant(value, now_instant, now_system)),
                        retry_count: snapshot.retry_count,
                        stale: snapshot.stale,
                        gap: snapshot.persistence_gap,
                    },
                );
            }
        }
        components
            .entry(TelemetryComponent::Management)
            .or_insert_with(|| ComponentHealth::ready("management", now_system));
        components
            .entry(TelemetryComponent::Runtime)
            .or_insert_with(|| ComponentHealth::ready("runtime", now_system));
        let components = components.into_values().collect::<Vec<_>>();
        let overall_status = components
            .iter()
            .map(|component| component.status)
            .max_by_key(|status| health_severity(status))
            .unwrap_or("healthy");
        HealthSnapshot {
            sampled_at: format_time(now_system),
            overall_status,
            components,
        }
    }

    async fn statistics(&self, params: StatisticsParams) -> Result<StatisticsPage, QueryError> {
        let query = params.validate()?;
        let revision = self.coordinator.load().revision().0.to_string();
        let result = self
            .storage
            .statistics(query, query_deadline())
            .await
            .map_err(|_| QueryError::Internal)?;
        Ok(StatisticsPage {
            page: query.page.page,
            page_size: query.page.page_size,
            total_items: result.total_items,
            sampled_at: now_rfc3339(),
            runtime_revision: revision,
            items: result
                .items
                .into_iter()
                .map(|item| StatisticItem {
                    date: format_epoch_day(item.day_utc),
                    dimension_kind: statistic_dimension_name(query.dimension),
                    dimension_value: item.dimension_value,
                    count: item.count,
                })
                .collect(),
        })
    }

    async fn queries(&self, params: QueryParams) -> Result<QueryPage, QueryError> {
        let query = params.validate()?;
        let revision = self.coordinator.load().revision().0.to_string();
        let result = self
            .storage
            .resolve_queries(query, query_deadline())
            .await
            .map_err(|_| QueryError::Internal)?;
        Ok(QueryPage {
            page: query.page.page,
            page_size: query.page.page_size,
            total_items: result.total_items,
            sampled_at: now_rfc3339(),
            runtime_revision: revision,
            items: result
                .items
                .into_iter()
                .map(query_record)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    fn resources(&self) -> ResourceSnapshot {
        let runtime = self.coordinator.load();
        let revision = runtime.revision().0.to_string();
        let items = runtime
            .snapshot()
            .resources()
            .summary()
            .into_iter()
            .map(|(id, summary)| {
                let version = summary.version();
                ResourceSummary {
                    id: id.as_str().to_owned(),
                    display_name: id.as_str().to_owned(),
                    epoch: version.epoch().to_string(),
                    revision: version.revision().to_string(),
                    source_kind: match summary.source_kind() {
                        ResourceSourceKind::Const => "const",
                        ResourceSourceKind::File => "file",
                        ResourceSourceKind::Remote => "remote",
                    },
                    fallback: summary.used_fallback(),
                    stale: summary.stale_status() == ResourceStaleStatus::Stale,
                }
            })
            .collect();
        ResourceSnapshot {
            sampled_at: now_rfc3339(),
            runtime_revision: revision,
            items,
        }
    }

    fn system(&self) -> SystemInfo {
        SystemInfo {
            version: env!("CARGO_PKG_VERSION"),
            started_at: format_time(self.started_at),
            uptime_seconds: Instant::now()
                .saturating_duration_since(self.started_instant)
                .as_secs(),
            capabilities: [
                "read:overview",
                "read:runtime",
                "read:health",
                "read:statistics",
                "read:queries",
                "read:resources",
                "read:system",
            ],
        }
    }
}

pub(crate) fn routes() -> Router<Arc<AuthServices>> {
    Router::new()
        .route("/api/v1/overview", get(get_overview))
        .route("/api/v1/runtime", get(get_runtime))
        .route("/api/v1/health", get(get_health))
        .route("/api/v1/statistics", get(get_statistics))
        .route("/api/v1/queries", get(get_queries))
        .route("/api/v1/resources", get(get_resources))
        .route("/api/v1/system", get(get_system))
        .route("/api/v2/service/metrics", get(get_service_metrics))
        .route("/api/v2/system/runtime", get(get_process_metrics))
        .route("/api/v2/config/state", get(get_config_state))
        .route("/api/v2/config/system", get(get_system_config))
        .route("/api/v2/config/modules/{module}", get(get_config_module))
        .route("/api/v2/retention", get(get_retention))
        .route("/api/v2/queries/search", post(history::post_query_search))
        .route(
            "/api/v2/queries/{record_id}",
            get(history::get_query_detail),
        )
}

async fn get_overview(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    match queries.overview().await {
        Ok(response) => Json(response).into_response(),
        Err(_) => internal_error(&request_id),
    }
}

async fn get_runtime(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    Json(queries.runtime()).into_response()
}

async fn get_health(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    Json(queries.health()).into_response()
}

async fn get_statistics(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    params: Result<Query<StatisticsParams>, QueryRejection>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    let Ok(Query(params)) = params else {
        return invalid_argument(&request_id);
    };
    match queries.statistics(params).await {
        Ok(response) => Json(response).into_response(),
        Err(QueryError::Invalid) => invalid_argument(&request_id),
        Err(QueryError::Internal) => internal_error(&request_id),
    }
}

async fn get_queries(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    params: Result<Query<QueryParams>, QueryRejection>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    let Ok(Query(params)) = params else {
        return invalid_argument(&request_id);
    };
    match queries.queries(params).await {
        Ok(response) => Json(response).into_response(),
        Err(QueryError::Invalid) => invalid_argument(&request_id),
        Err(QueryError::Internal) => internal_error(&request_id),
    }
}

async fn get_resources(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    Json(queries.resources()).into_response()
}

async fn get_system(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let Some(queries) = &services.queries else {
        return internal_error(&request_id);
    };
    Json(queries.system()).into_response()
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

#[derive(Clone, Copy, Debug)]
enum QueryError {
    Invalid,
    Internal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatisticsParams {
    date_from: String,
    date_to: String,
    dimension: StatisticDimensionParam,
    #[serde(default = "default_page")]
    page: u32,
    #[serde(default = "default_page_size")]
    page_size: u32,
}

impl StatisticsParams {
    fn validate(self) -> Result<StatisticsQuery, QueryError> {
        let day_from = parse_date(&self.date_from).ok_or(QueryError::Invalid)?;
        let day_to = parse_date(&self.date_to).ok_or(QueryError::Invalid)?;
        if day_to < day_from || day_to - day_from + 1 > MAX_STATISTIC_DAYS {
            return Err(QueryError::Invalid);
        }
        Ok(StatisticsQuery {
            day_from,
            day_to,
            dimension: self.dimension.into(),
            page: validate_page(self.page, self.page_size)?,
        })
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StatisticDimensionParam {
    Total,
    Transport,
    Source,
    Rcode,
    Outcome,
    Cache,
}

impl From<StatisticDimensionParam> for StatisticDimension {
    fn from(value: StatisticDimensionParam) -> Self {
        match value {
            StatisticDimensionParam::Total => Self::Total,
            StatisticDimensionParam::Transport => Self::Transport,
            StatisticDimensionParam::Source => Self::Source,
            StatisticDimensionParam::Rcode => Self::Rcode,
            StatisticDimensionParam::Outcome => Self::Outcome,
            StatisticDimensionParam::Cache => Self::Cache,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryParams {
    #[serde(default = "default_page")]
    page: u32,
    #[serde(default = "default_page_size")]
    page_size: u32,
    transport: Option<QueryTransportParam>,
    source: Option<QuerySourceParam>,
    rcode: Option<QueryRcodeParam>,
    outcome: Option<QueryOutcomeParam>,
    #[serde(default)]
    sort: QuerySortParam,
    #[serde(default)]
    order: SortOrderParam,
}

impl QueryParams {
    fn validate(self) -> Result<ResolveQuery, QueryError> {
        Ok(ResolveQuery {
            page: validate_page(self.page, self.page_size)?,
            transport: self.transport.map(Into::into),
            source: self.source.map(Into::into),
            rcode: self.rcode.map(Into::into),
            outcome: self.outcome.map(Into::into),
            sort: self.sort.into(),
            order: self.order.into(),
        })
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QueryTransportParam {
    Udp,
    Tcp,
    Doh,
}

impl From<QueryTransportParam> for QueryTransport {
    fn from(value: QueryTransportParam) -> Self {
        match value {
            QueryTransportParam::Udp => Self::Udp,
            QueryTransportParam::Tcp => Self::Tcp,
            QueryTransportParam::Doh => Self::Doh,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QuerySourceParam {
    Cache,
    Hosts,
    Rule,
    Upstream,
    Synthetic,
}

impl From<QuerySourceParam> for QuerySource {
    fn from(value: QuerySourceParam) -> Self {
        match value {
            QuerySourceParam::Cache => Self::Cache,
            QuerySourceParam::Hosts => Self::Hosts,
            QuerySourceParam::Rule => Self::Rule,
            QuerySourceParam::Upstream => Self::Upstream,
            QuerySourceParam::Synthetic => Self::Synthetic,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
enum QueryRcodeParam {
    #[serde(rename = "NOERROR")]
    NoError,
    #[serde(rename = "FORMERR")]
    FormErr,
    #[serde(rename = "SERVFAIL")]
    ServFail,
    #[serde(rename = "NXDOMAIN")]
    NxDomain,
    #[serde(rename = "NOTIMP")]
    NotImp,
    #[serde(rename = "REFUSED")]
    Refused,
    #[serde(rename = "OTHER")]
    Other,
}

impl From<QueryRcodeParam> for QueryRcode {
    fn from(value: QueryRcodeParam) -> Self {
        match value {
            QueryRcodeParam::NoError => Self::NoError,
            QueryRcodeParam::FormErr => Self::FormErr,
            QueryRcodeParam::ServFail => Self::ServFail,
            QueryRcodeParam::NxDomain => Self::NxDomain,
            QueryRcodeParam::NotImp => Self::NotImp,
            QueryRcodeParam::Refused => Self::Refused,
            QueryRcodeParam::Other => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QueryOutcomeParam {
    Answered,
    Negative,
    Timeout,
    Rejected,
    Failed,
}

impl From<QueryOutcomeParam> for QueryOutcome {
    fn from(value: QueryOutcomeParam) -> Self {
        match value {
            QueryOutcomeParam::Answered => Self::Answered,
            QueryOutcomeParam::Negative => Self::Negative,
            QueryOutcomeParam::Timeout => Self::Timeout,
            QueryOutcomeParam::Rejected => Self::Rejected,
            QueryOutcomeParam::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QuerySortParam {
    #[default]
    OccurredAt,
    DurationMs,
}

impl From<QuerySortParam> for QuerySort {
    fn from(value: QuerySortParam) -> Self {
        match value {
            QuerySortParam::OccurredAt => Self::OccurredAt,
            QuerySortParam::DurationMs => Self::DurationMillis,
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SortOrderParam {
    Asc,
    #[default]
    Desc,
}

impl From<SortOrderParam> for SortOrder {
    fn from(value: SortOrderParam) -> Self {
        match value {
            SortOrderParam::Asc => Self::Asc,
            SortOrderParam::Desc => Self::Desc,
        }
    }
}

#[derive(Serialize)]
struct Overview {
    sampled_at: String,
    runtime_revision: String,
    overall_status: &'static str,
    cards: Vec<OverviewCard>,
    resolution_pipeline: ResolutionPipelineStatus,
}

#[derive(Serialize)]
struct ResolutionPipelineStatus {
    accepted: u64,
    dropped: u64,
    gap_started_at_utc_millis: Option<u64>,
    cache_commit_stored: u64,
    cache_commit_rejected: u64,
    cache_commit_conflict: u64,
    cache_commit_unavailable: u64,
    cache_commit_dropped: u64,
    detail_accepted: u64,
    detail_dropped: u64,
    detail_failed: u64,
}

impl From<ResolutionPipelineSnapshot> for ResolutionPipelineStatus {
    fn from(snapshot: ResolutionPipelineSnapshot) -> Self {
        Self {
            accepted: snapshot.accepted,
            dropped: snapshot.dropped,
            gap_started_at_utc_millis: snapshot.gap_started_at_utc_millis,
            cache_commit_stored: snapshot.cache_commit_stored,
            cache_commit_rejected: snapshot.cache_commit_rejected,
            cache_commit_conflict: snapshot.cache_commit_conflict,
            cache_commit_unavailable: snapshot.cache_commit_unavailable,
            cache_commit_dropped: snapshot.cache_commit_dropped,
            detail_accepted: snapshot.detail_accepted,
            detail_dropped: snapshot.detail_dropped,
            detail_failed: snapshot.detail_failed,
        }
    }
}

#[derive(Serialize)]
struct OverviewCard {
    key: &'static str,
    label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<&'static str>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason_code: Option<&'static str>,
}

impl OverviewCard {
    fn available(key: &'static str, label: &'static str, value: f64, unit: &'static str) -> Self {
        Self {
            key,
            label,
            value: Some(value),
            unit: Some(unit),
            status: "available",
            unavailable_reason_code: None,
        }
    }

    fn unavailable(key: &'static str, label: &'static str, reason: &'static str) -> Self {
        Self {
            key,
            label,
            value: None,
            unit: None,
            status: "unavailable",
            unavailable_reason_code: Some(reason),
        }
    }
}

#[derive(Serialize)]
struct RuntimeSnapshot {
    sampled_at: String,
    revision: String,
    normalized_hash: String,
    listener_count: usize,
    bind_count: usize,
    resource_count: usize,
    has_policy_core: bool,
    binds: Vec<BindEntry>,
}

#[derive(Serialize)]
struct BindEntry {
    transport: &'static str,
    address: String,
    port: u16,
    owner: String,
    v6_only: bool,
    state: &'static str,
}

#[derive(Serialize)]
struct HealthSnapshot {
    sampled_at: String,
    overall_status: &'static str,
    components: Vec<ComponentHealth>,
}

#[derive(Serialize)]
struct ComponentHealth {
    component: &'static str,
    status: &'static str,
    reason_code: &'static str,
    first_changed_at: Option<String>,
    last_changed_at: String,
    last_success_at: Option<String>,
    retry_count: u64,
    stale: bool,
    gap: bool,
}

impl ComponentHealth {
    fn ready(component: &'static str, now: SystemTime) -> Self {
        let now = format_time(now);
        Self {
            component,
            status: "healthy",
            reason_code: "READY",
            first_changed_at: None,
            last_changed_at: now.clone(),
            last_success_at: Some(now),
            retry_count: 0,
            stale: false,
            gap: false,
        }
    }
}

#[derive(Serialize)]
struct StatisticsPage {
    page: u32,
    page_size: u32,
    total_items: u64,
    sampled_at: String,
    runtime_revision: String,
    items: Vec<StatisticItem>,
}

#[derive(Serialize)]
struct StatisticItem {
    date: String,
    dimension_kind: &'static str,
    dimension_value: String,
    count: u64,
}

#[derive(Serialize)]
struct QueryPage {
    page: u32,
    page_size: u32,
    total_items: u64,
    sampled_at: String,
    runtime_revision: String,
    items: Vec<QueryRecord>,
}

#[derive(Serialize)]
struct QueryRecord {
    id: String,
    occurred_at: String,
    duration_ms: u64,
    dns_core_duration_ms: Option<f64>,
    transport: &'static str,
    source: &'static str,
    rcode: &'static str,
    outcome: &'static str,
    cache: &'static str,
    policy_matched: bool,
    resource_matched: bool,
    detail_status: &'static str,
    qname: Option<String>,
    qtype: String,
    client_name: Option<String>,
    client_ip: Option<String>,
    strategy_id: Option<String>,
    upstream_target_id: Option<String>,
    upstream_used_id: Option<String>,
    answer_count: Option<u32>,
    answers_truncated: Option<bool>,
    answers: Option<Vec<QueryAnswer>>,
}

#[derive(Serialize)]
struct QueryAnswer {
    name: String,
    #[serde(rename = "type")]
    record_type: String,
    ttl: u32,
    data: String,
}

#[derive(Serialize)]
struct ResourceSnapshot {
    sampled_at: String,
    runtime_revision: String,
    items: Vec<ResourceSummary>,
}

#[derive(Serialize)]
struct ResourceSummary {
    id: String,
    display_name: String,
    epoch: String,
    revision: String,
    source_kind: &'static str,
    fallback: bool,
    stale: bool,
}

#[derive(Serialize)]
struct SystemInfo {
    version: &'static str,
    started_at: String,
    uptime_seconds: u64,
    capabilities: [&'static str; 7],
}

fn validate_page(page: u32, page_size: u32) -> Result<PageRequest, QueryError> {
    if page == 0 || page_size == 0 || page_size > MAX_PAGE_SIZE {
        return Err(QueryError::Invalid);
    }
    Ok(PageRequest { page, page_size })
}

fn parse_date(value: &str) -> Option<i32> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::from_calendar_date(year, month, day)
        .ok()?
        .to_julian_day()
        .checked_sub(UNIX_EPOCH_JULIAN_DAY)
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

fn query_record(value: ResolveQueryRecord) -> Result<QueryRecord, QueryError> {
    let millis = u64::try_from(value.occurred_at_millis).map_err(|_| QueryError::Internal)?;
    let occurred_at = UNIX_EPOCH
        .checked_add(Duration::from_millis(millis))
        .ok_or(QueryError::Internal)?;
    Ok(QueryRecord {
        id: value.id,
        occurred_at: format_time(occurred_at),
        duration_ms: value.duration_millis,
        dns_core_duration_ms: value
            .dns_core_duration_micros
            .map(|micros| Duration::from_micros(micros).as_secs_f64() * 1_000.0),
        transport: query_transport_name(value.transport),
        source: query_source_name(value.source),
        rcode: query_rcode_name(value.rcode),
        outcome: query_outcome_name(value.outcome),
        cache: query_cache_name(value.cache),
        policy_matched: value.policy_matched,
        resource_matched: value.resource_matched,
        detail_status: match value.detail_status {
            QueryDetailStatus::Available => "available",
            QueryDetailStatus::LegacyRedacted => "legacy_redacted",
        },
        qname: value.qname,
        qtype: value.qtype,
        client_name: value.client_name,
        client_ip: value.client_ip,
        strategy_id: value.strategy_id,
        upstream_target_id: value.upstream_target_id,
        upstream_used_id: value.upstream_used_id,
        answer_count: value.answer_count,
        answers_truncated: value.answers_truncated,
        answers: value.answers.map(|answers| {
            answers
                .into_iter()
                .map(|answer| QueryAnswer {
                    name: answer.name,
                    record_type: answer.record_type,
                    ttl: answer.ttl,
                    data: answer.data,
                })
                .collect()
        }),
    })
}

fn query_deadline() -> Deadline {
    Deadline::new(Instant::now() + QUERY_TIMEOUT)
}

fn unix_millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

fn unix_millis_u64(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn format_instant(value: Instant, now: Instant, now_system: SystemTime) -> String {
    format_time(
        now_system
            .checked_sub(now.saturating_duration_since(value))
            .unwrap_or(UNIX_EPOCH),
    )
}

fn now_rfc3339() -> String {
    format_time(SystemTime::now())
}

fn format_time(time: SystemTime) -> String {
    OffsetDateTime::from(time)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn bind_transport_name(value: BindTransport) -> &'static str {
    match value {
        BindTransport::Udp => "udp",
        BindTransport::Tcp => "tcp",
        BindTransport::Doh => "doh",
    }
}

fn component_name(value: TelemetryComponent) -> &'static str {
    match value {
        TelemetryComponent::Application => "application",
        TelemetryComponent::Runtime => "runtime",
        TelemetryComponent::Listener => "listener",
        TelemetryComponent::Dns => "dns",
        TelemetryComponent::Resolution => "resolution",
        TelemetryComponent::Policy => "policy",
        TelemetryComponent::Upstream => "upstream",
        TelemetryComponent::Cache => "cache",
        TelemetryComponent::Resource => "resource",
        TelemetryComponent::Storage => "storage",
        TelemetryComponent::Telemetry => "telemetry",
        TelemetryComponent::Management => "management",
    }
}

fn health_status(value: ComponentHealthState) -> &'static str {
    match value {
        ComponentHealthState::Healthy => "healthy",
        ComponentHealthState::Degraded => "degraded",
        ComponentHealthState::Failed => "failed",
        ComponentHealthState::Stopping => "stopping",
    }
}

fn health_reason_code(
    state: ComponentHealthState,
    safe_reason: Option<&'static str>,
) -> &'static str {
    match safe_reason {
        Some("telemetry output unavailable") => "TELEMETRY_OUTPUT_UNAVAILABLE",
        Some("resolve detail queue is full") => "RESOLVE_DETAIL_QUEUE_FULL",
        Some("cache persistence shutdown has gaps") => "CACHE_PERSISTENCE_GAP",
        _ => match state {
            ComponentHealthState::Healthy => "READY",
            ComponentHealthState::Degraded => "DEGRADED",
            ComponentHealthState::Failed => "FAILED",
            ComponentHealthState::Stopping => "STOPPING",
        },
    }
}

fn health_severity(value: &str) -> u8 {
    match value {
        "failed" => 4,
        "degraded" => 3,
        "stopping" => 2,
        _ => 1,
    }
}

fn statistic_dimension_name(value: StatisticDimension) -> &'static str {
    match value {
        StatisticDimension::Total => "total",
        StatisticDimension::Transport => "transport",
        StatisticDimension::Source => "source",
        StatisticDimension::Rcode => "rcode",
        StatisticDimension::Outcome => "outcome",
        StatisticDimension::Cache => "cache",
    }
}

fn query_transport_name(value: QueryTransport) -> &'static str {
    match value {
        QueryTransport::Udp => "udp",
        QueryTransport::Tcp => "tcp",
        QueryTransport::Doh => "doh",
    }
}

fn query_source_name(value: QuerySource) -> &'static str {
    match value {
        QuerySource::Cache => "cache",
        QuerySource::Hosts => "hosts",
        QuerySource::Rule => "rule",
        QuerySource::Upstream => "upstream",
        QuerySource::Synthetic => "synthetic",
    }
}

fn query_rcode_name(value: QueryRcode) -> &'static str {
    match value {
        QueryRcode::NoError => "NOERROR",
        QueryRcode::FormErr => "FORMERR",
        QueryRcode::ServFail => "SERVFAIL",
        QueryRcode::NxDomain => "NXDOMAIN",
        QueryRcode::NotImp => "NOTIMP",
        QueryRcode::Refused => "REFUSED",
        QueryRcode::Other => "OTHER",
    }
}

fn query_outcome_name(value: QueryOutcome) -> &'static str {
    match value {
        QueryOutcome::Answered => "answered",
        QueryOutcome::Negative => "negative",
        QueryOutcome::Timeout => "timeout",
        QueryOutcome::Rejected => "rejected",
        QueryOutcome::Failed => "failed",
    }
}

fn query_cache_name(value: QueryCacheOutcome) -> &'static str {
    match value {
        QueryCacheOutcome::Hit => "hit",
        QueryCacheOutcome::Stale => "stale",
        QueryCacheOutcome::Miss => "miss",
        QueryCacheOutcome::Bypass => "bypass",
    }
}

const fn default_page() -> u32 {
    DEFAULT_PAGE
}

const fn default_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use axum::body::{Body, to_bytes};
    use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
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
    use crate::ports::management::{
        OverviewCounters, ResolveQueryResult, StatisticRecord, StatisticsResult,
    };
    use crate::ports::observation::ClientMatchSource;
    use crate::ports::storage::{ResolveAnswer, ResolveEvent, StatsSource};
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::ports::{PortError, PortErrorClass, PortFuture};
    use crate::runtime::{PreparedRuntime, RuntimeCoordinator, bind_prepared};

    const HISTORY_DAY: u64 = 20_710;

    fn history_record(
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

    struct FakeReadModel;

    impl ManagementStorageRead for FakeReadModel {
        fn overview(
            &self,
            _since_utc_millis: i64,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<OverviewCounters, PortError>> {
            Box::pin(async {
                Ok(OverviewCounters {
                    queries: 20,
                    failed: 2,
                    cache_hits: 15,
                })
            })
        }

        fn statistics(
            &self,
            query: StatisticsQuery,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<StatisticsResult, PortError>> {
            Box::pin(async move {
                Ok(StatisticsResult {
                    total_items: 1,
                    items: vec![StatisticRecord {
                        day_utc: query.day_from,
                        dimension_value: "all".to_owned(),
                        count: 20,
                    }],
                })
            })
        }

        fn resolve_queries(
            &self,
            _query: ResolveQuery,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<crate::ports::management::ResolveQueryResult, PortError>>
        {
            Box::pin(async {
                Ok(ResolveQueryResult {
                    total_items: 1,
                    items: vec![ResolveQueryRecord {
                        id: "qry_opaque".to_owned(),
                        occurred_at_millis: 0,
                        duration_millis: 8,
                        dns_core_duration_micros: Some(345),
                        transport: QueryTransport::Doh,
                        source: QuerySource::Rule,
                        rcode: QueryRcode::NoError,
                        outcome: QueryOutcome::Answered,
                        cache: QueryCacheOutcome::Miss,
                        policy_matched: true,
                        resource_matched: false,
                        detail_status: QueryDetailStatus::Available,
                        qname: Some("example.test.".to_owned()),
                        qtype: "A".to_owned(),
                        client_name: Some("office".to_owned()),
                        client_ip: Some("192.0.2.10".to_owned()),
                        strategy_id: Some("default".to_owned()),
                        upstream_target_id: Some("public-dns".to_owned()),
                        upstream_used_id: Some("alidns".to_owned()),
                        answer_count: Some(1),
                        answers_truncated: Some(false),
                        answers: Some(vec![crate::ports::management::QueryAnswer {
                            name: "example.test.".to_owned(),
                            record_type: "A".to_owned(),
                            ttl: 60,
                            data: "192.0.2.20".to_owned(),
                        }]),
                    }],
                })
            })
        }
    }

    #[derive(Clone, Copy)]
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

    async fn test_services() -> (Arc<AuthServices>, PathBuf) {
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
            Arc::new(FakeReadModel),
            None,
            true,
            Arc::new(ResolutionPipelineMetrics::default()),
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

    #[test]
    fn validates_statistics_date_range_and_paging() {
        let valid = StatisticsParams {
            date_from: "2026-08-01".to_owned(),
            date_to: "2026-08-31".to_owned(),
            dimension: StatisticDimensionParam::Total,
            page: 1,
            page_size: 100,
        }
        .validate()
        .unwrap();
        assert_eq!(valid.day_to - valid.day_from + 1, 31);
        assert_eq!(format_epoch_day(valid.day_from), "2026-08-01");

        let invalid = StatisticsParams {
            date_from: "2026-08-01".to_owned(),
            date_to: "2026-09-01".to_owned(),
            dimension: StatisticDimensionParam::Total,
            page: 1,
            page_size: 20,
        };
        assert!(matches!(invalid.validate(), Err(QueryError::Invalid)));
        assert!(validate_page(0, 20).is_err());
        assert!(validate_page(1, 101).is_err());
    }

    #[test]
    fn query_projection_contains_only_the_openapi_safe_fields() {
        let value = query_record(ResolveQueryRecord {
            id: "qry_opaque".to_owned(),
            occurred_at_millis: 0,
            duration_millis: 8,
            dns_core_duration_micros: Some(345),
            transport: QueryTransport::Doh,
            source: QuerySource::Rule,
            rcode: QueryRcode::NoError,
            outcome: QueryOutcome::Answered,
            cache: QueryCacheOutcome::Miss,
            policy_matched: true,
            resource_matched: false,
            detail_status: QueryDetailStatus::Available,
            qname: Some("example.test.".to_owned()),
            qtype: "A".to_owned(),
            client_name: Some("office".to_owned()),
            client_ip: Some("192.0.2.10".to_owned()),
            strategy_id: Some("default".to_owned()),
            upstream_target_id: Some("public-dns".to_owned()),
            upstream_used_id: Some("alidns".to_owned()),
            answer_count: Some(1),
            answers_truncated: Some(false),
            answers: Some(vec![crate::ports::management::QueryAnswer {
                name: "example.test.".to_owned(),
                record_type: "A".to_owned(),
                ttl: 60,
                data: "192.0.2.20".to_owned(),
            }]),
        })
        .unwrap();
        let value = serde_json::to_value(value).unwrap();
        assert_eq!(value["occurred_at"], "1970-01-01T00:00:00Z");
        assert_eq!(value["duration_ms"], 8);
        assert_eq!(value["dns_core_duration_ms"], 0.345);
        assert_eq!(value["source"], "rule");
        assert_eq!(value.as_object().unwrap().len(), 22);
        assert_eq!(value["detail_status"], "available");
        assert_eq!(value["qname"], "example.test.");
        assert_eq!(value["client_ip"], "192.0.2.10");
        assert_eq!(value["upstream_used_id"], "alidns");
        assert_eq!(value["answers"][0]["data"], "192.0.2.20");
        for forbidden in [
            "canonical_qname",
            "client_bucket",
            "request_digest",
            "route_id",
            "password_hash",
        ] {
            assert!(value.get(forbidden).is_none());
        }
    }

    #[tokio::test]
    async fn authenticated_router_serves_all_read_only_contracts() {
        let (services, root) = test_services().await;
        let issued = services.sessions.issue("admin".to_owned()).unwrap();
        let authorization = format!("Bearer {}", issued.view.access_token);
        let app = build_router(Arc::clone(&services));
        let paths = [
            "/api/v1/overview",
            "/api/v1/runtime",
            "/api/v1/health",
            "/api/v1/statistics?date_from=1970-01-01&date_to=1970-01-01&dimension=total",
            "/api/v1/queries?transport=doh&source=rule&rcode=NOERROR&outcome=answered",
            "/api/v1/resources",
            "/api/v1/system",
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
            if path == "/api/v1/overview" {
                let pipeline = body
                    .get("resolution_pipeline")
                    .and_then(serde_json::Value::as_object)
                    .expect("overview must expose the low-cardinality resolution pipeline");
                for counter in [
                    "accepted",
                    "dropped",
                    "cache_commit_stored",
                    "cache_commit_rejected",
                    "cache_commit_conflict",
                    "cache_commit_unavailable",
                    "cache_commit_dropped",
                    "detail_accepted",
                    "detail_dropped",
                    "detail_failed",
                ] {
                    assert_eq!(
                        pipeline.get(counter).and_then(|value| value.as_u64()),
                        Some(0)
                    );
                }
                assert!(pipeline.contains_key("gap_started_at_utc_millis"));
            }
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
            if !path.starts_with("/api/v1/queries") {
                for query_only in ["qname", "client_ip", "answers"] {
                    assert!(!serialized.contains(query_only), "{path}: {query_only}");
                }
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

        let unauthorized = app
            .clone()
            .oneshot(get("/api/v1/overview", None))
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

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
            "/api/v2/retention/preview",
        ] {
            let request = Request::builder()
                .method("POST")
                .uri(path)
                .header(AUTHORIZATION, &authorization)
                .body(Body::from("{}"))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }

        let invalid = app
            .clone()
            .oneshot(get(
                "/api/v1/statistics?date_from=2026-08-01&date_to=2026-09-01&dimension=total",
                Some(&authorization),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(invalid.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "INVALID_ARGUMENT");

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
