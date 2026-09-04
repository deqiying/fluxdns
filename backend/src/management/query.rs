//! Management API 的只读查询、参数校验与安全响应投影。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};

use super::router::{AuthServices, RequestId, internal_error, invalid_argument};
use crate::config::BindTransport;
use crate::dns::Deadline;
use crate::observability::TelemetryWriter;
use crate::ports::management::{
    ManagementStorageRead, PageRequest, QueryCacheOutcome, QueryDetailStatus, QueryOutcome,
    QueryRcode, QuerySort, QuerySource, QueryTransport, ResolveQuery, ResolveQueryRecord,
    SortOrder, StatisticDimension, StatisticsQuery,
};
use crate::ports::telemetry::{Component as TelemetryComponent, ComponentHealthState};
use crate::resource::{ResourceSourceKind, ResourceStaleStatus};
use crate::runtime::RuntimeCoordinator;

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
}

impl ManagementQueryService {
    pub(crate) fn new(
        coordinator: Arc<RuntimeCoordinator>,
        storage: Arc<dyn ManagementStorageRead>,
        telemetry: Option<Arc<TelemetryWriter>>,
        resolve_log_enabled: bool,
    ) -> Self {
        Self {
            coordinator,
            storage,
            telemetry,
            started_at: SystemTime::now(),
            started_instant: Instant::now(),
            resolve_log_enabled,
        }
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
    use axum::http::header::{CONTENT_TYPE, COOKIE};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::config::migrate::deterministic_hash;
    use crate::config::store::ConfigStore;
    use crate::config::{ConfigLoader, LoadOptions};
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
    use crate::ports::{PortError, PortErrorClass, PortFuture};
    use crate::runtime::{PreparedRuntime, RuntimeCoordinator, bind_prepared};

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
        let (source, work_path) = crate::config::test_support::portable_example();
        let output = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
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
        let query_service = Arc::new(ManagementQueryService::new(
            Arc::new(RuntimeCoordinator::new(candidate)),
            Arc::new(FakeReadModel),
            None,
            true,
        ));
        let root = work_path.with_extension("management-query-router");
        std::fs::create_dir_all(&root).unwrap();
        let source_path = root.join("config.yaml");
        std::fs::write(&source_path, source.as_bytes()).unwrap();
        let auth = Arc::new(AuthState::new(&[]).unwrap());
        let sessions = Arc::new(SessionStore::new(false));
        let store = Arc::new(ConfigStore::new(
            source_path.clone(),
            source_path,
            deterministic_hash(source.as_bytes()),
        ));
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

    fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
        let mut request = Request::builder().uri(path);
        if let Some(cookie) = cookie {
            request = request.header(COOKIE, cookie);
        }
        request.body(Body::empty()).unwrap()
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
        assert_eq!(value["source"], "rule");
        assert_eq!(value.as_object().unwrap().len(), 21);
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
        let cookie = format!("{}={}", services.sessions.cookie_name(), issued.token);
        let app = build_router(Arc::clone(&services));
        let paths = [
            "/api/v1/overview",
            "/api/v1/runtime",
            "/api/v1/health",
            "/api/v1/statistics?date_from=1970-01-01&date_to=1970-01-01&dimension=total",
            "/api/v1/queries?transport=doh&source=rule&rcode=NOERROR&outcome=answered",
            "/api/v1/resources",
            "/api/v1/system",
        ];
        for path in paths {
            let response = app.clone().oneshot(get(path, Some(&cookie))).await.unwrap();
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

        let unauthorized = app
            .clone()
            .oneshot(get("/api/v1/overview", None))
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let invalid = app
            .oneshot(get(
                "/api/v1/statistics?date_from=2026-08-01&date_to=2026-09-01&dimension=total",
                Some(&cookie),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(invalid.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "INVALID_ARGUMENT");

        let _ = std::fs::remove_dir_all(root);
    }
}
