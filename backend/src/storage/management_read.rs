//! Management API 使用的独立只读 SQLite adapter。

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use crate::dns::Deadline;
use crate::ports::management::{
    ManagementStorageRead, OverviewCounters, QueryAnswer, QueryCacheOutcome, QueryDetailStatus,
    QueryOutcome, QueryRcode, QuerySort, QuerySource, QueryTransport, ResolveQuery,
    ResolveQueryRecord, ResolveQueryResult, SortOrder, StatisticDimension, StatisticRecord,
    StatisticsQuery, StatisticsResult,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

const READ_BUSY_TIMEOUT: Duration = Duration::from_millis(2_000);

const OVERVIEW_SQL: &str = "SELECT COUNT(*) AS queries, \
     COALESCE(SUM(CASE WHEN failure_class IS NOT NULL OR rcode = 2 THEN 1 ELSE 0 END), 0) AS failed, \
     COALESCE(SUM(CASE WHEN cache_status IN ('fresh', 'stale') THEN 1 ELSE 0 END), 0) AS cache_hits \
     FROM resolve_log \
     WHERE transport IS NOT NULL AND CAST(event_time_utc AS INTEGER) >= ?";
const TOTAL_COUNT_SQL: &str =
    "SELECT COUNT(*) FROM stats_daily_total WHERE day_utc BETWEEN ? AND ?";
const TOTAL_PAGE_SQL: &str = "SELECT day_utc, total_requests AS count FROM stats_daily_total \
     WHERE day_utc BETWEEN ? AND ? ORDER BY day_utc DESC LIMIT ? OFFSET ?";
const DIMENSION_COUNT_SQL: &str = "SELECT COUNT(*) FROM stats_daily_dimension \
     WHERE day_utc BETWEEN ? AND ? AND dimension_kind = ?";
const DIMENSION_PAGE_SQL: &str = "SELECT day_utc, dimension_value, count FROM stats_daily_dimension \
     WHERE day_utc BETWEEN ? AND ? AND dimension_kind = ? \
     ORDER BY day_utc DESC, dimension_value ASC LIMIT ? OFFSET ?";
const QUERY_COUNT_SQL: &str = "SELECT COUNT(*) FROM resolve_log WHERE transport IS NOT NULL \
     AND source IN ('cache', 'hosts', 'rule_set', 'upstream') \
     AND (?1 IS NULL OR transport = ?1) \
     AND (?2 IS NULL OR source = ?2) \
     AND (?3 IS NULL OR CASE rcode WHEN 0 THEN 'NOERROR' WHEN 1 THEN 'FORMERR' \
          WHEN 2 THEN 'SERVFAIL' WHEN 3 THEN 'NXDOMAIN' WHEN 4 THEN 'NOTIMP' \
          WHEN 5 THEN 'REFUSED' ELSE 'OTHER' END = ?3) \
     AND (?4 IS NULL OR CASE WHEN failure_class = 'timeout' THEN 'timeout' \
          WHEN failure_class IS NOT NULL THEN 'failed' WHEN rcode = 3 THEN 'negative' \
          WHEN rcode IN (1, 4, 5) THEN 'rejected' WHEN rcode = 2 THEN 'failed' \
          ELSE 'answered' END = ?4)";
macro_rules! query_page_sql {
    ($order_by:literal) => {
        concat!(
            "SELECT id, CAST(event_time_utc AS INTEGER) AS occurred_at_millis, duration_millis, ",
            "transport, CASE source WHEN 'rule_set' THEN 'rule' ELSE source END AS source, ",
            "CASE rcode WHEN 0 THEN 'NOERROR' WHEN 1 THEN 'FORMERR' WHEN 2 THEN 'SERVFAIL' ",
            "WHEN 3 THEN 'NXDOMAIN' WHEN 4 THEN 'NOTIMP' WHEN 5 THEN 'REFUSED' ELSE 'OTHER' END AS rcode, ",
            "CASE WHEN failure_class = 'timeout' THEN 'timeout' WHEN failure_class IS NOT NULL THEN 'failed' ",
            "WHEN rcode = 3 THEN 'negative' WHEN rcode IN (1, 4, 5) THEN 'rejected' ",
            "WHEN rcode = 2 THEN 'failed' ELSE 'answered' END AS outcome, ",
            "CASE cache_status WHEN 'fresh' THEN 'hit' WHEN 'stale' THEN 'stale' ",
            "WHEN 'miss' THEN 'miss' ELSE 'bypass' END AS cache, ",
            "CASE WHEN strategy_id IS NOT NULL OR matched_rule_source IS NOT NULL THEN 1 ELSE 0 END AS policy_matched, ",
            "CASE WHEN matched_resource_id IS NOT NULL THEN 1 ELSE 0 END AS resource_matched, ",
            "canonical_qname, qtype, client_bucket, client_ip, strategy_id, upstream_id, ",
            "upstream_used_id, answer_count, answers_truncated, answer_summary_json ",
            "FROM resolve_log WHERE transport IS NOT NULL AND source IN ('cache', 'hosts', 'rule_set', 'upstream') ",
            "AND (?1 IS NULL OR transport = ?1) AND (?2 IS NULL OR source = ?2) ",
            "AND (?3 IS NULL OR CASE rcode WHEN 0 THEN 'NOERROR' WHEN 1 THEN 'FORMERR' ",
            "WHEN 2 THEN 'SERVFAIL' WHEN 3 THEN 'NXDOMAIN' WHEN 4 THEN 'NOTIMP' ",
            "WHEN 5 THEN 'REFUSED' ELSE 'OTHER' END = ?3) ",
            "AND (?4 IS NULL OR CASE WHEN failure_class = 'timeout' THEN 'timeout' ",
            "WHEN failure_class IS NOT NULL THEN 'failed' WHEN rcode = 3 THEN 'negative' ",
            "WHEN rcode IN (1, 4, 5) THEN 'rejected' WHEN rcode = 2 THEN 'failed' ",
            "ELSE 'answered' END = ?4) ",
            $order_by,
            " LIMIT ?5 OFFSET ?6"
        )
    };
}

// 排序只从四个编译期模板中选择，不接受用户提供的 SQL 片段。
const QUERY_OCCURRED_ASC_SQL: &str =
    query_page_sql!("ORDER BY CAST(event_time_utc AS INTEGER) ASC, id ASC");
const QUERY_OCCURRED_DESC_SQL: &str =
    query_page_sql!("ORDER BY CAST(event_time_utc AS INTEGER) DESC, id DESC");
const QUERY_DURATION_ASC_SQL: &str = query_page_sql!("ORDER BY duration_millis ASC, id ASC");
const QUERY_DURATION_DESC_SQL: &str = query_page_sql!("ORDER BY duration_millis DESC, id DESC");

pub struct SqliteManagementReadModel {
    pool: SqlitePool,
    opaque_id_key: [u8; 32],
}

#[derive(Debug, Error)]
pub enum SqliteManagementReadModelBuildError {
    #[error("management read model database could not be opened")]
    Connect,
    #[error("management read model entropy source is unavailable")]
    Entropy,
}

impl SqliteManagementReadModel {
    pub async fn connect(
        path: impl Into<PathBuf>,
    ) -> Result<Self, SqliteManagementReadModelBuildError> {
        let options = SqliteConnectOptions::new()
            .filename(path.into())
            .read_only(true)
            .busy_timeout(READ_BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .map_err(|_| SqliteManagementReadModelBuildError::Connect)?;
        let mut opaque_id_key = [0_u8; 32];
        getrandom::fill(&mut opaque_id_key)
            .map_err(|_| SqliteManagementReadModelBuildError::Entropy)?;
        Ok(Self {
            pool,
            opaque_id_key,
        })
    }

    fn opaque_id(&self, row_id: i64) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.opaque_id_key);
        hasher.update(row_id.to_be_bytes());
        let digest = hasher.finalize();
        format!("qry_{}", URL_SAFE_NO_PAD.encode(&digest[..12]))
    }

    async fn overview_now(&self, since_utc_millis: i64) -> Result<OverviewCounters, PortError> {
        let row = sqlx::query(OVERVIEW_SQL)
            .bind(since_utc_millis)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| read_error("management_read.overview"))?;
        Ok(OverviewCounters {
            queries: nonnegative_u64(&row, "queries", "management_read.overview")?,
            failed: nonnegative_u64(&row, "failed", "management_read.overview")?,
            cache_hits: nonnegative_u64(&row, "cache_hits", "management_read.overview")?,
        })
    }

    async fn statistics_now(&self, query: StatisticsQuery) -> Result<StatisticsResult, PortError> {
        let (limit, offset) = page_bounds(query.page.page, query.page.page_size)?;
        let (total_items, items) = if query.dimension == StatisticDimension::Total {
            let total = sqlx::query_scalar::<_, i64>(TOTAL_COUNT_SQL)
                .bind(i64::from(query.day_from))
                .bind(i64::from(query.day_to))
                .fetch_one(&self.pool)
                .await
                .map_err(|_| read_error("management_read.statistics"))?;
            let rows = sqlx::query(TOTAL_PAGE_SQL)
                .bind(i64::from(query.day_from))
                .bind(i64::from(query.day_to))
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await
                .map_err(|_| read_error("management_read.statistics"))?;
            let items = rows
                .into_iter()
                .map(|row| statistic_row(&row, "all"))
                .collect::<Result<Vec<_>, _>>()?;
            (to_u64(total, "management_read.statistics")?, items)
        } else {
            let kind = dimension_database_name(query.dimension);
            let total = sqlx::query_scalar::<_, i64>(DIMENSION_COUNT_SQL)
                .bind(i64::from(query.day_from))
                .bind(i64::from(query.day_to))
                .bind(kind)
                .fetch_one(&self.pool)
                .await
                .map_err(|_| read_error("management_read.statistics"))?;
            let rows = sqlx::query(DIMENSION_PAGE_SQL)
                .bind(i64::from(query.day_from))
                .bind(i64::from(query.day_to))
                .bind(kind)
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await
                .map_err(|_| read_error("management_read.statistics"))?;
            let items = rows
                .into_iter()
                .map(|row| {
                    let value = row
                        .try_get::<String, _>("dimension_value")
                        .map_err(|_| read_error("management_read.statistics"))?;
                    statistic_row(&row, &value)
                })
                .collect::<Result<Vec<_>, _>>()?;
            (to_u64(total, "management_read.statistics")?, items)
        };
        Ok(StatisticsResult { total_items, items })
    }

    async fn resolve_queries_now(
        &self,
        query: ResolveQuery,
    ) -> Result<ResolveQueryResult, PortError> {
        let (limit, offset) = page_bounds(query.page.page, query.page.page_size)?;
        let transport = query.transport.map(transport_database_name);
        let source = query.source.map(source_database_name);
        let rcode = query.rcode.map(rcode_name);
        let outcome = query.outcome.map(outcome_name);
        let total = sqlx::query_scalar::<_, i64>(QUERY_COUNT_SQL)
            .bind(transport)
            .bind(source)
            .bind(rcode)
            .bind(outcome)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| read_error("management_read.queries"))?;
        let sql = match (query.sort, query.order) {
            (QuerySort::OccurredAt, SortOrder::Asc) => QUERY_OCCURRED_ASC_SQL,
            (QuerySort::OccurredAt, SortOrder::Desc) => QUERY_OCCURRED_DESC_SQL,
            (QuerySort::DurationMillis, SortOrder::Asc) => QUERY_DURATION_ASC_SQL,
            (QuerySort::DurationMillis, SortOrder::Desc) => QUERY_DURATION_DESC_SQL,
        };
        let rows = sqlx::query(sql)
            .bind(transport)
            .bind(source)
            .bind(rcode)
            .bind(outcome)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(|_| read_error("management_read.queries"))?;
        let items = rows
            .into_iter()
            .map(|row| self.resolve_query_row(&row))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ResolveQueryResult {
            total_items: to_u64(total, "management_read.queries")?,
            items,
        })
    }

    fn resolve_query_row(
        &self,
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<ResolveQueryRecord, PortError> {
        let operation = "management_read.queries";
        let row_id = row
            .try_get::<i64, _>("id")
            .map_err(|_| read_error(operation))?;
        let qtype = row
            .try_get::<i64, _>("qtype")
            .map_err(|_| read_error(operation))?;
        let qtype = u16::try_from(qtype).map_err(|_| read_error(operation))?;
        let persisted_answer_count = row
            .try_get::<Option<i64>, _>("answer_count")
            .map_err(|_| read_error(operation))?;
        let (
            detail_status,
            qname,
            client_name,
            client_ip,
            strategy_id,
            upstream_target_id,
            upstream_used_id,
            answer_count,
            answers_truncated,
            answers,
        ) = if let Some(answer_count) = persisted_answer_count {
            let answer_count = u32::try_from(answer_count).map_err(|_| read_error(operation))?;
            let qname = required_detail_text(row, "canonical_qname", operation)?;
            if qname.starts_with("len:") {
                return Err(read_error(operation));
            }
            let client_name = optional_detail_text(row, "client_bucket", operation)?;
            let client_ip = optional_detail_text(row, "client_ip", operation)?
                .map(|value| {
                    value
                        .parse::<IpAddr>()
                        .map(|address| address.to_string())
                        .map_err(|_| read_error(operation))
                })
                .transpose()?;
            let strategy_id = optional_detail_text(row, "strategy_id", operation)?;
            let upstream_target_id = optional_detail_text(row, "upstream_id", operation)?;
            let upstream_used_id = optional_detail_text(row, "upstream_used_id", operation)?;
            let answers_truncated = match row
                .try_get::<Option<i64>, _>("answers_truncated")
                .map_err(|_| read_error(operation))?
            {
                Some(0) => false,
                Some(1) => true,
                _ => return Err(read_error(operation)),
            };
            let answer_summary_json = required_detail_text(row, "answer_summary_json", operation)?;
            if answer_summary_json.len() > 4_096 {
                return Err(read_error(operation));
            }
            let answers = serde_json::from_str::<Vec<QueryAnswer>>(&answer_summary_json)
                .map_err(|_| read_error(operation))?;
            if answers.len() > 16
                || answer_count < u32::try_from(answers.len()).unwrap_or(u32::MAX)
                || (!answers_truncated
                    && answer_count != u32::try_from(answers.len()).unwrap_or(u32::MAX))
            {
                return Err(read_error(operation));
            }
            (
                QueryDetailStatus::Available,
                Some(qname),
                client_name,
                client_ip,
                strategy_id,
                upstream_target_id,
                upstream_used_id,
                Some(answer_count),
                Some(answers_truncated),
                Some(answers),
            )
        } else {
            (
                QueryDetailStatus::LegacyRedacted,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        };
        Ok(ResolveQueryRecord {
            id: self.opaque_id(row_id),
            occurred_at_millis: row
                .try_get("occurred_at_millis")
                .map_err(|_| read_error(operation))?,
            duration_millis: nonnegative_u64(row, "duration_millis", operation)?,
            transport: parse_transport(
                &row.try_get::<String, _>("transport")
                    .map_err(|_| read_error(operation))?,
            )?,
            source: parse_source(
                &row.try_get::<String, _>("source")
                    .map_err(|_| read_error(operation))?,
            )?,
            rcode: parse_rcode(
                &row.try_get::<String, _>("rcode")
                    .map_err(|_| read_error(operation))?,
            )?,
            outcome: parse_outcome(
                &row.try_get::<String, _>("outcome")
                    .map_err(|_| read_error(operation))?,
            )?,
            cache: parse_cache(
                &row.try_get::<String, _>("cache")
                    .map_err(|_| read_error(operation))?,
            )?,
            policy_matched: row
                .try_get::<i64, _>("policy_matched")
                .map_err(|_| read_error(operation))?
                != 0,
            resource_matched: row
                .try_get::<i64, _>("resource_matched")
                .map_err(|_| read_error(operation))?
                != 0,
            detail_status,
            qname,
            qtype: query_type_name(qtype),
            client_name,
            client_ip,
            strategy_id,
            upstream_target_id,
            upstream_used_id,
            answer_count,
            answers_truncated,
            answers,
        })
    }
}

impl ManagementStorageRead for SqliteManagementReadModel {
    fn overview(
        &self,
        since_utc_millis: i64,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<OverviewCounters, PortError>> {
        Box::pin(with_deadline(
            deadline,
            "management_read.overview",
            self.overview_now(since_utc_millis),
        ))
    }

    fn statistics(
        &self,
        query: StatisticsQuery,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<StatisticsResult, PortError>> {
        Box::pin(with_deadline(
            deadline,
            "management_read.statistics",
            self.statistics_now(query),
        ))
    }

    fn resolve_queries(
        &self,
        query: ResolveQuery,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<ResolveQueryResult, PortError>> {
        Box::pin(with_deadline(
            deadline,
            "management_read.queries",
            self.resolve_queries_now(query),
        ))
    }
}

async fn with_deadline<T>(
    deadline: Deadline,
    operation: &'static str,
    future: impl std::future::Future<Output = Result<T, PortError>>,
) -> Result<T, PortError> {
    let now = Instant::now();
    if deadline.is_expired(now) {
        return Err(PortError::new(PortErrorClass::Timeout, operation));
    }
    tokio::time::timeout(deadline.remaining(now), future)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Timeout, operation))?
}

fn page_bounds(page: u32, page_size: u32) -> Result<(i64, i64), PortError> {
    if page == 0 || page_size == 0 || page_size > 100 {
        return Err(PortError::new(
            PortErrorClass::InvalidInput,
            "management_read.page",
        ));
    }
    let offset = u64::from(page.saturating_sub(1))
        .checked_mul(u64::from(page_size))
        .ok_or_else(|| PortError::new(PortErrorClass::ResourceExhausted, "management_read.page"))?;
    Ok((
        i64::from(page_size),
        i64::try_from(offset).map_err(|_| {
            PortError::new(PortErrorClass::ResourceExhausted, "management_read.page")
        })?,
    ))
}

fn statistic_row(row: &sqlx::sqlite::SqliteRow, value: &str) -> Result<StatisticRecord, PortError> {
    let operation = "management_read.statistics";
    let day = row
        .try_get::<i64, _>("day_utc")
        .map_err(|_| read_error(operation))?;
    Ok(StatisticRecord {
        day_utc: i32::try_from(day).map_err(|_| read_error(operation))?,
        dimension_value: value.to_owned(),
        count: nonnegative_u64(row, "count", operation)?,
    })
}

fn dimension_database_name(value: StatisticDimension) -> &'static str {
    match value {
        StatisticDimension::Total => "total",
        StatisticDimension::Transport => "transport",
        StatisticDimension::Source => "source",
        StatisticDimension::Rcode => "rcode",
        StatisticDimension::Outcome => "attempt_outcome",
        StatisticDimension::Cache => "cache_status",
    }
}

fn transport_database_name(value: QueryTransport) -> &'static str {
    match value {
        QueryTransport::Udp => "udp",
        QueryTransport::Tcp => "tcp",
        QueryTransport::Doh => "doh",
    }
}

fn source_database_name(value: QuerySource) -> &'static str {
    match value {
        QuerySource::Cache => "cache",
        QuerySource::Hosts => "hosts",
        QuerySource::Rule => "rule_set",
        QuerySource::Upstream => "upstream",
        QuerySource::Synthetic => "synthetic",
    }
}

fn rcode_name(value: QueryRcode) -> &'static str {
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

fn outcome_name(value: QueryOutcome) -> &'static str {
    match value {
        QueryOutcome::Answered => "answered",
        QueryOutcome::Negative => "negative",
        QueryOutcome::Timeout => "timeout",
        QueryOutcome::Rejected => "rejected",
        QueryOutcome::Failed => "failed",
    }
}

fn required_detail_text(
    row: &sqlx::sqlite::SqliteRow,
    column: &'static str,
    operation: &'static str,
) -> Result<String, PortError> {
    optional_detail_text(row, column, operation)?.ok_or_else(|| read_error(operation))
}

fn optional_detail_text(
    row: &sqlx::sqlite::SqliteRow,
    column: &'static str,
    operation: &'static str,
) -> Result<Option<String>, PortError> {
    let value = row
        .try_get::<Option<String>, _>(column)
        .map_err(|_| read_error(operation))?;
    if value
        .as_deref()
        .is_some_and(|value| value.is_empty() || value == "<present>" || value == "<absent>")
    {
        return Err(read_error(operation));
    }
    Ok(value)
}

fn query_type_name(value: u16) -> String {
    let record_type = hickory_proto::rr::RecordType::from(value);
    if matches!(record_type, hickory_proto::rr::RecordType::Unknown(_)) {
        format!("TYPE{value}")
    } else {
        record_type.to_string()
    }
}

fn parse_transport(value: &str) -> Result<QueryTransport, PortError> {
    match value {
        "udp" => Ok(QueryTransport::Udp),
        "tcp" => Ok(QueryTransport::Tcp),
        "doh" => Ok(QueryTransport::Doh),
        _ => Err(read_error("management_read.queries")),
    }
}

fn parse_source(value: &str) -> Result<QuerySource, PortError> {
    match value {
        "cache" => Ok(QuerySource::Cache),
        "hosts" => Ok(QuerySource::Hosts),
        "rule" => Ok(QuerySource::Rule),
        "upstream" => Ok(QuerySource::Upstream),
        "synthetic" => Ok(QuerySource::Synthetic),
        _ => Err(read_error("management_read.queries")),
    }
}

fn parse_rcode(value: &str) -> Result<QueryRcode, PortError> {
    match value {
        "NOERROR" => Ok(QueryRcode::NoError),
        "FORMERR" => Ok(QueryRcode::FormErr),
        "SERVFAIL" => Ok(QueryRcode::ServFail),
        "NXDOMAIN" => Ok(QueryRcode::NxDomain),
        "NOTIMP" => Ok(QueryRcode::NotImp),
        "REFUSED" => Ok(QueryRcode::Refused),
        "OTHER" => Ok(QueryRcode::Other),
        _ => Err(read_error("management_read.queries")),
    }
}

fn parse_outcome(value: &str) -> Result<QueryOutcome, PortError> {
    match value {
        "answered" => Ok(QueryOutcome::Answered),
        "negative" => Ok(QueryOutcome::Negative),
        "timeout" => Ok(QueryOutcome::Timeout),
        "rejected" => Ok(QueryOutcome::Rejected),
        "failed" => Ok(QueryOutcome::Failed),
        _ => Err(read_error("management_read.queries")),
    }
}

fn parse_cache(value: &str) -> Result<QueryCacheOutcome, PortError> {
    match value {
        "hit" => Ok(QueryCacheOutcome::Hit),
        "stale" => Ok(QueryCacheOutcome::Stale),
        "miss" => Ok(QueryCacheOutcome::Miss),
        "bypass" => Ok(QueryCacheOutcome::Bypass),
        _ => Err(read_error("management_read.queries")),
    }
}

fn nonnegative_u64(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
    operation: &'static str,
) -> Result<u64, PortError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|_| read_error(operation))?;
    to_u64(value, operation)
}

fn to_u64(value: i64, operation: &'static str) -> Result<u64, PortError> {
    u64::try_from(value).map_err(|_| read_error(operation))
}

fn read_error(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Unavailable, operation)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use crate::dns::{CancelReason, Deadline, RuntimeRevision, TransportClass};
    use crate::ports::management::{
        ManagementStorageRead, PageRequest, QueryCacheOutcome, QueryDetailStatus, QueryOutcome,
        QueryRcode, QuerySort, QuerySource, QueryTransport, ResolveQuery, SortOrder,
        StatisticDimension, StatisticsQuery,
    };
    use crate::ports::storage::{
        ResolveAnswer, ResolveEvent, ResolveRuleSource, StatsBatch, StatsDimension, StatsEvent,
        StatsSource, StorageBackend, StorageOperation, StorageTransaction,
    };
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::storage::{STORAGE_SCHEMA_VERSION, SqliteStorageBackend};

    use super::SqliteManagementReadModel;

    static NEXT_DB: AtomicU64 = AtomicU64::new(0);

    fn database_path() -> std::path::PathBuf {
        let id = NEXT_DB.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fluxdns-management-read-{}-{id}.sqlite3",
            std::process::id()
        ))
    }

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    async fn seeded_database() -> (
        std::path::PathBuf,
        SqliteStorageBackend,
        SqliteManagementReadModel,
    ) {
        let path = database_path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let stats = StatsEvent::new(
            1,
            20_000,
            vec![
                StatsDimension::transport(TransportClass::Multiplexed),
                StatsDimension::source(StatsSource::Upstream),
                StatsDimension::rcode(2),
                StatsDimension::cache_status(CacheStatus::Disabled),
                StatsDimension::attempt_outcome(OutcomeClass::Timeout),
            ],
        )
        .unwrap();
        let detail = ResolveEvent {
            occurred_at: SystemTime::now(),
            duration_started_at: Instant::now()
                .checked_sub(Duration::from_millis(25))
                .unwrap(),
            request_digest: Arc::from("private-digest"),
            listener_id: Arc::from("doh-listener"),
            route_id: Some(Arc::from("private-route")),
            client_ip: Some("192.0.2.25".parse().unwrap()),
            client_bucket: Some(Arc::from("private-client")),
            strategy_id: Some(Arc::from("default")),
            upstream_id: Some(Arc::from("remote")),
            upstream_member_id: None,
            upstream_used_id: Some(Arc::from("remote")),
            matched_rule_source: Some(ResolveRuleSource::RuleSet),
            matched_resource_id: Some(Arc::from("geosite")),
            matched_rule_ordinal: Some(7),
            resource_version: None,
            transport: TransportClass::Multiplexed,
            qname: Arc::from("private.example."),
            qtype: 1,
            qclass: 1,
            answers: vec![ResolveAnswer {
                name: "private.example.".to_owned(),
                record_type: "A".to_owned(),
                data: "192.0.2.80".to_owned(),
                ttl: 60,
            }],
            rcode: 2,
            cancellation_reason: Some(CancelReason::DeadlineExceeded),
            outcome: OutcomeClass::Timeout,
            source: StatsSource::Upstream,
            cache_status: CacheStatus::Disabled,
            runtime_revision: RuntimeRevision(9),
        };
        backend
            .execute(
                StorageTransaction {
                    idempotency_key: Arc::from("management-read-seed"),
                    operations: vec![
                        StorageOperation::StatsBatch(StatsBatch {
                            batch_id: 1,
                            max_event_sequence: 1,
                            counter_epoch: 0,
                            events: vec![stats],
                        }),
                        StorageOperation::ResolveBatch(vec![detail]),
                    ],
                },
                deadline(),
            )
            .await
            .unwrap();
        let read_model = SqliteManagementReadModel::connect(&path).await.unwrap();
        (path, backend, read_model)
    }

    #[tokio::test]
    async fn reads_paginated_statistics_and_filtered_safe_query_projection() {
        let (path, backend, read_model) = seeded_database().await;
        let statistics = read_model
            .statistics(
                StatisticsQuery {
                    day_from: 20_000,
                    day_to: 20_000,
                    dimension: StatisticDimension::Total,
                    page: PageRequest {
                        page: 1,
                        page_size: 20,
                    },
                },
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(statistics.total_items, 1);
        assert_eq!(statistics.items[0].count, 1);

        let queries = read_model
            .resolve_queries(
                ResolveQuery {
                    page: PageRequest {
                        page: 1,
                        page_size: 20,
                    },
                    transport: Some(QueryTransport::Doh),
                    source: Some(QuerySource::Upstream),
                    rcode: Some(QueryRcode::ServFail),
                    outcome: Some(QueryOutcome::Timeout),
                    sort: QuerySort::DurationMillis,
                    order: SortOrder::Desc,
                },
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(queries.total_items, 1);
        let item = &queries.items[0];
        assert!(item.id.starts_with("qry_"));
        assert_ne!(item.id, "qry_1");
        assert_eq!(item.transport, QueryTransport::Doh);
        assert_eq!(item.source, QuerySource::Upstream);
        assert_eq!(item.rcode, QueryRcode::ServFail);
        assert_eq!(item.outcome, QueryOutcome::Timeout);
        assert_eq!(item.cache, QueryCacheOutcome::Bypass);
        assert!(item.policy_matched);
        assert!(item.resource_matched);
        assert_eq!(item.detail_status, QueryDetailStatus::Available);
        assert_eq!(item.qname.as_deref(), Some("private.example."));
        assert_eq!(item.qtype, "A");
        assert_eq!(item.client_name.as_deref(), Some("private-client"));
        assert_eq!(item.client_ip.as_deref(), Some("192.0.2.25"));
        assert_eq!(item.strategy_id.as_deref(), Some("default"));
        assert_eq!(item.upstream_target_id.as_deref(), Some("remote"));
        assert_eq!(item.upstream_used_id.as_deref(), Some("remote"));
        assert_eq!(item.answer_count, Some(1));
        assert_eq!(item.answers_truncated, Some(false));
        assert_eq!(item.answers.as_ref().unwrap()[0].data, "192.0.2.80");

        read_model.pool.close().await;
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn read_model_connection_rejects_writes_and_reports_overview() {
        let (path, backend, read_model) = seeded_database().await;
        let overview = read_model.overview(0, deadline()).await.unwrap();
        assert_eq!(overview.queries, 1);
        assert_eq!(overview.failed, 1);
        assert_eq!(overview.cache_hits, 0);
        assert!(
            sqlx::query("DELETE FROM resolve_log")
                .execute(&read_model.pool)
                .await
                .is_err()
        );

        read_model.pool.close().await;
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn pre_v4_detail_is_exposed_as_legacy_without_placeholders() {
        let path = database_path();
        let backend = SqliteStorageBackend::connect(&path).await.unwrap();
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO resolve_log \
             (event_time_utc, duration_millis, request_id_digest, listener_id, client_bucket, \
              strategy_id, canonical_qname, qtype, qclass, source, upstream_id, rcode, \
              cache_status, runtime_revision, transport) \
             VALUES ('0', 1, '<present>', 'listener', '<present>', '<present>', 'len:12', \
                     28, 1, 'upstream', '<present>', 0, 'miss', 1, 'udp')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let read_model = SqliteManagementReadModel::connect(&path).await.unwrap();
        let result = read_model
            .resolve_queries(
                ResolveQuery {
                    page: PageRequest {
                        page: 1,
                        page_size: 20,
                    },
                    transport: None,
                    source: None,
                    rcode: None,
                    outcome: None,
                    sort: QuerySort::OccurredAt,
                    order: SortOrder::Desc,
                },
                deadline(),
            )
            .await
            .unwrap();
        let item = &result.items[0];
        assert_eq!(item.detail_status, QueryDetailStatus::LegacyRedacted);
        assert_eq!(item.qtype, "AAAA");
        assert_eq!(item.qname, None);
        assert_eq!(item.client_name, None);
        assert_eq!(item.client_ip, None);
        assert_eq!(item.strategy_id, None);
        assert_eq!(item.upstream_target_id, None);
        assert_eq!(item.upstream_used_id, None);
        assert_eq!(item.answers, None);

        read_model.pool.close().await;
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[tokio::test]
    async fn malformed_answer_json_fails_the_query_instead_of_faking_an_empty_result() {
        let (path, backend, read_model) = seeded_database().await;
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query("UPDATE resolve_log SET answer_summary_json = 'not-json'")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let error = read_model
            .resolve_queries(
                ResolveQuery {
                    page: PageRequest {
                        page: 1,
                        page_size: 20,
                    },
                    transport: None,
                    source: None,
                    rcode: None,
                    outcome: None,
                    sort: QuerySort::OccurredAt,
                    order: SortOrder::Desc,
                },
                deadline(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Unavailable
        ));

        read_model.pool.close().await;
        backend.shutdown(deadline()).await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
