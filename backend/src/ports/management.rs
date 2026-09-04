//! Management 查询使用的稳定只读 port 与安全投影。

use crate::dns::Deadline;

use super::{PortError, PortFuture};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatisticDimension {
    Total,
    Transport,
    Source,
    Rcode,
    Outcome,
    Cache,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryTransport {
    Udp,
    Tcp,
    Doh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuerySource {
    Cache,
    Hosts,
    Rule,
    Upstream,
    Synthetic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryRcode {
    NoError,
    FormErr,
    ServFail,
    NxDomain,
    NotImp,
    Refused,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryOutcome {
    Answered,
    Negative,
    Timeout,
    Rejected,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryCacheOutcome {
    Hit,
    Stale,
    Miss,
    Bypass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuerySort {
    OccurredAt,
    DurationMillis,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageRequest {
    pub page: u32,
    pub page_size: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatisticsQuery {
    pub day_from: i32,
    pub day_to: i32,
    pub dimension: StatisticDimension,
    pub page: PageRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatisticRecord {
    pub day_utc: i32,
    pub dimension_value: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatisticsResult {
    pub total_items: u64,
    pub items: Vec<StatisticRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolveQuery {
    pub page: PageRequest,
    pub transport: Option<QueryTransport>,
    pub source: Option<QuerySource>,
    pub rcode: Option<QueryRcode>,
    pub outcome: Option<QueryOutcome>,
    pub sort: QuerySort,
    pub order: SortOrder,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolveQueryRecord {
    pub id: String,
    pub occurred_at_millis: i64,
    pub duration_millis: u64,
    pub transport: QueryTransport,
    pub source: QuerySource,
    pub rcode: QueryRcode,
    pub outcome: QueryOutcome,
    pub cache: QueryCacheOutcome,
    pub policy_matched: bool,
    pub resource_matched: bool,
    pub detail_status: QueryDetailStatus,
    pub qname: Option<String>,
    pub qtype: String,
    pub client_name: Option<String>,
    pub client_ip: Option<String>,
    pub strategy_id: Option<String>,
    pub upstream_target_id: Option<String>,
    pub upstream_used_id: Option<String>,
    pub answer_count: Option<u32>,
    pub answers_truncated: Option<bool>,
    pub answers: Option<Vec<QueryAnswer>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryDetailStatus {
    Available,
    LegacyRedacted,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct QueryAnswer {
    pub name: String,
    #[serde(rename = "type")]
    pub record_type: String,
    pub ttl: u32,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolveQueryResult {
    pub total_items: u64,
    pub items: Vec<ResolveQueryRecord>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OverviewCounters {
    pub queries: u64,
    pub failed: u64,
    pub cache_hits: u64,
}

pub trait ManagementStorageRead: Send + Sync {
    fn overview(
        &self,
        since_utc_millis: i64,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<OverviewCounters, PortError>>;

    fn statistics(
        &self,
        query: StatisticsQuery,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<StatisticsResult, PortError>>;

    fn resolve_queries(
        &self,
        query: ResolveQuery,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<ResolveQueryResult, PortError>>;
}
