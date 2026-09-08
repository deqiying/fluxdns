//! v2 解析历史 HTTP 适配；过滤和分页由日分片 storage 执行，当前名称仅做读取投影。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection};
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Response};
use hickory_proto::rr::{Name, RecordType};

use super::ManagementQueryService;
use crate::config::contract::ConfigV2;
use crate::config::store::ConfigStore;
use crate::management::contract::{
    self, AnswerSummary, CacheOutcome, CacheProducer, CommitCursor, Cursor, DecimalU64, DnsAnswer,
    ErrorCode, HistoricalMatch, PageDirection, QueryDetail, QueryFilter, QueryOutcome, QueryPage,
    QueryRecord, QueryRequest, QuerySort, QuerySource, RecordId, RequestIdentity, Revision,
    SortOrder, Transport,
};
use crate::management::router::{AuthServices, RequestId, v2_error_response};
use crate::ports::observation::ClientMatchSource;
use crate::ports::{PortError, PortErrorClass};
use crate::storage::{
    DetailCommittedRecord, DetailPageDirection, DetailQuery, DetailQueryCacheOutcome,
    DetailQueryFilter, DetailQueryOutcome, DetailQueryRcode, DetailQueryRecord, DetailQuerySort,
    DetailQuerySource, DetailQueryTransport, DetailRecordId, DetailSortOrder,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(super) async fn post_query_search(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Some(queries) = &services.queries else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let Ok(body) = body else {
        return v2_error_response(ErrorCode::InvalidArgument, &request_id);
    };
    let request = match contract::decode_query(&body) {
        Ok(request) => request,
        Err(code) => return v2_error_response(code, &request_id),
    };
    match search(queries, &services.config_store, request).await {
        Ok(page) => Json(page).into_response(),
        Err(code) => v2_error_response(code, &request_id),
    }
}

pub(super) async fn get_query_detail(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    record_id: Result<Path<String>, PathRejection>,
) -> Response {
    let Some(queries) = &services.queries else {
        return v2_error_response(ErrorCode::ServiceUnavailable, &request_id);
    };
    let Ok(Path(record_id)) = record_id else {
        return v2_error_response(ErrorCode::InvalidArgument, &request_id);
    };
    match detail(queries, &services.config_store, record_id).await {
        Ok(detail) => Json(detail).into_response(),
        Err(code) => v2_error_response(code, &request_id),
    }
}

async fn search(
    service: &ManagementQueryService,
    store: &ConfigStore,
    request: QueryRequest,
) -> Result<QueryPage, ErrorCode> {
    let directory = directory_snapshot(service, store)?;
    let query = storage_query(request, &directory.config)?;
    let page = service
        .detail_store
        .query_details(query, super::query_deadline())
        .await
        .map_err(storage_error)?;
    Ok(QueryPage {
        items: page
            .items
            .into_iter()
            .map(|record| query_record(record, &directory.names))
            .collect::<Result<Vec<_>, _>>()?,
        previous_cursor: page
            .previous_cursor
            .map(Cursor::try_from)
            .transpose()
            .map_err(|_| ErrorCode::ServiceUnavailable)?,
        next_cursor: page
            .next_cursor
            .map(Cursor::try_from)
            .transpose()
            .map_err(|_| ErrorCode::ServiceUnavailable)?,
        snapshot_cursor: CommitCursor {
            epoch: Revision::try_from(page.snapshot_cursor.epoch)
                .map_err(|_| ErrorCode::ServiceUnavailable)?,
            sequence: DecimalU64::from(page.snapshot_cursor.sequence),
        },
        directory_revision: directory.revision,
        retention_revision: Revision::try_from(page.retention_revision.to_string())
            .map_err(|_| ErrorCode::ServiceUnavailable)?,
        available_from_ms: page.available_from_utc_millis,
    })
}

async fn detail(
    service: &ManagementQueryService,
    store: &ConfigStore,
    record_id: String,
) -> Result<QueryDetail, ErrorCode> {
    let directory = directory_snapshot(service, store)?;
    let storage_id = DetailRecordId::try_from(record_id).map_err(storage_error)?;
    let record = service
        .detail_store
        .read_detail(&storage_id, super::query_deadline())
        .await
        .map_err(storage_error)?
        .ok_or(ErrorCode::NotFound)?;
    Ok(QueryDetail {
        record: query_record(record, &directory.names)?,
        directory_revision: directory.revision,
    })
}

/// 将 commit 后通知按 HTTP 相同的目录快照、规范化和过滤规则投影为 WS 记录。
pub(super) fn project_committed_records(
    service: &ManagementQueryService,
    store: &ConfigStore,
    filter: QueryFilter,
    records: &[DetailCommittedRecord],
) -> Result<(Revision, Vec<QueryRecord>), ErrorCode> {
    let directory = directory_snapshot(service, store)?;
    let matched_client_ids = filter.client_name.as_ref().map_or_else(Vec::new, |name| {
        let needle = name.to_ascii_lowercase();
        directory
            .config
            .clients
            .iter()
            .filter(|client| client.name.to_ascii_lowercase().contains(&needle))
            .map(|client| client.client_id.clone())
            .collect()
    });
    let require_matched_client_ids = filter.client_name.is_some();
    let filter = storage_filter(filter, matched_client_ids, require_matched_client_ids)?;
    let items = records
        .iter()
        .map(DetailCommittedRecord::to_query_record)
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_error)?
        .into_iter()
        .filter(|record| filter.matches_record(record))
        .map(|record| query_record(record, &directory.names))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((directory.revision, items))
}

struct DirectorySnapshot {
    revision: Revision,
    config: Arc<ConfigV2>,
    names: BTreeMap<String, String>,
}

/// 当前名称不写回历史；一次请求只消费一个活动目录快照。
fn directory_snapshot(
    service: &ManagementQueryService,
    store: &ConfigStore,
) -> Result<DirectorySnapshot, ErrorCode> {
    let active = store
        .active_snapshot()
        .map_err(|_| ErrorCode::ServiceUnavailable)?;
    if active.runtime_revision != service.coordinator.load().revision().0 {
        return Err(ErrorCode::ServiceUnavailable);
    }
    let names = active
        .config
        .clients
        .iter()
        .map(|client| (client.client_id.clone(), client.name.clone()))
        .collect();
    Ok(DirectorySnapshot {
        revision: Revision::try_from(active.revision).map_err(|_| ErrorCode::ServiceUnavailable)?,
        config: active.config,
        names,
    })
}

fn storage_query(request: QueryRequest, directory: &ConfigV2) -> Result<DetailQuery, ErrorCode> {
    let QueryRequest {
        filter,
        cursor,
        direction,
        page_size,
        sort,
        order,
    } = request;
    let matched_client_ids = filter.client_name.as_ref().map_or_else(Vec::new, |name| {
        let needle = name.to_ascii_lowercase();
        directory
            .clients
            .iter()
            .filter(|client| client.name.to_ascii_lowercase().contains(&needle))
            .map(|client| client.client_id.clone())
            .collect()
    });
    let require_matched_client_ids = filter.client_name.is_some();
    Ok(DetailQuery {
        filter: storage_filter(filter, matched_client_ids, require_matched_client_ids)?,
        cursor: cursor.map(|value| value.as_str().to_owned()),
        direction: match direction {
            PageDirection::Older => DetailPageDirection::Older,
            PageDirection::Newer => DetailPageDirection::Newer,
        },
        page_size,
        sort: match sort {
            QuerySort::OccurredAt => DetailQuerySort::OccurredAt,
            QuerySort::Duration => DetailQuerySort::Duration,
        },
        order: match order {
            SortOrder::Asc => DetailSortOrder::Asc,
            SortOrder::Desc => DetailSortOrder::Desc,
        },
    })
}

fn storage_filter(
    filter: QueryFilter,
    matched_client_ids: Vec<String>,
    require_matched_client_ids: bool,
) -> Result<DetailQueryFilter, ErrorCode> {
    Ok(DetailQueryFilter {
        from_utc_millis: filter.from_ms,
        to_utc_millis: filter.to_ms,
        client_id: filter.client_id,
        client_ip: filter
            .client_ip
            .map(|value| {
                value
                    .parse()
                    .map(normalize_ip)
                    .map(|value| value.to_string())
                    .map_err(|_| ErrorCode::InvalidArgument)
            })
            .transpose()?,
        qname: filter
            .qname
            .map(|value| canonical_qname(&value))
            .transpose()?,
        transport: filter.transport.map(|value| match value {
            Transport::Udp => DetailQueryTransport::Udp,
            Transport::Tcp => DetailQueryTransport::Tcp,
            Transport::Doh => DetailQueryTransport::Doh,
        }),
        matched_client_id: filter.matched_client_id,
        matched_client_ids,
        require_matched_client_ids,
        qtype: filter.qtype.map(|value| parse_qtype(&value)).transpose()?,
        rcode: filter.rcode.map(|value| parse_rcode(&value)).transpose()?,
        source: filter.source.map(|value| match value {
            QuerySource::Cache => DetailQuerySource::Cache,
            QuerySource::Hosts => DetailQuerySource::Hosts,
            QuerySource::Rule => DetailQuerySource::Rule,
            QuerySource::Upstream => DetailQuerySource::Upstream,
            QuerySource::Synthetic => DetailQuerySource::Synthetic,
        }),
        outcome: filter.outcome.map(|value| match value {
            QueryOutcome::Answered => DetailQueryOutcome::Answered,
            QueryOutcome::Negative => DetailQueryOutcome::Negative,
            QueryOutcome::Timeout => DetailQueryOutcome::Timeout,
            QueryOutcome::Rejected => DetailQueryOutcome::Rejected,
            QueryOutcome::Failed => DetailQueryOutcome::Failed,
        }),
        cache: filter.cache.map(|value| match value {
            CacheOutcome::Hit => DetailQueryCacheOutcome::Hit,
            CacheOutcome::Stale => DetailQueryCacheOutcome::Stale,
            CacheOutcome::Miss => DetailQueryCacheOutcome::Miss,
            CacheOutcome::Bypass => DetailQueryCacheOutcome::Bypass,
        }),
    })
}

fn query_record(
    record: DetailQueryRecord,
    current_names: &BTreeMap<String, String>,
) -> Result<QueryRecord, ErrorCode> {
    if record.occurred_at_millis > MAX_SAFE_INTEGER
        || record
            .dns_core_duration_micros
            .is_some_and(|value| value > MAX_SAFE_INTEGER)
    {
        return Err(ErrorCode::ServiceUnavailable);
    }
    let duration_us = record
        .duration_millis
        .checked_mul(1_000)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or(ErrorCode::ServiceUnavailable)?;
    let matched = match (
        record.client_match_source,
        record.matched_client_id.as_ref(),
    ) {
        (Some(ClientMatchSource::Id), Some(id)) => HistoricalMatch::Id {
            matched_client_id: id.clone(),
        },
        (Some(ClientMatchSource::Ip), Some(id)) => HistoricalMatch::Ip {
            matched_client_id: id.clone(),
        },
        (None, None) => HistoricalMatch::None {},
        _ => return Err(ErrorCode::ServiceUnavailable),
    };
    let current_client_name = record
        .matched_client_id
        .as_ref()
        .and_then(|id| current_names.get(id))
        .cloned();
    let cache_source = record.source == DetailQuerySource::Cache;
    let cache_producer = cache_source.then(|| CacheProducer {
        strategy_name: record.strategy_id.clone(),
        upstream_target_name: record.upstream_target_id.clone(),
        upstream_used_name: record.upstream_used_id.clone(),
    });
    let (upstream_target_name, upstream_used_name) = if cache_source {
        (None, None)
    } else {
        (
            record.upstream_target_id.clone(),
            record.upstream_used_id.clone(),
        )
    };
    let answers = record
        .answers
        .into_iter()
        .map(|answer| DnsAnswer {
            name: answer.name,
            r#type: answer.record_type,
            ttl_seconds: answer.ttl,
            data: answer.data,
        })
        .collect();
    let answers = if record.answers_truncated {
        AnswerSummary::Truncated {
            records: answers,
            total_count: record.answer_count,
        }
    } else {
        AnswerSummary::Available {
            records: answers,
            total_count: record.answer_count,
        }
    };
    Ok(QueryRecord {
        id: RecordId::try_from(record.id.as_str().to_owned())
            .map_err(|_| ErrorCode::ServiceUnavailable)?,
        occurred_at_ms: record.occurred_at_millis,
        identity: RequestIdentity {
            client_id: record.client_id,
            client_ip: record.client_ip.ok_or(ErrorCode::ServiceUnavailable)?,
        },
        matched,
        current_client_name,
        qname: record.qname,
        qtype: qtype_name(record.qtype),
        transport: match record.transport {
            DetailQueryTransport::Udp => Transport::Udp,
            DetailQueryTransport::Tcp => Transport::Tcp,
            DetailQueryTransport::Doh => Transport::Doh,
        },
        rcode: rcode_name(record.rcode),
        source: match record.source {
            DetailQuerySource::Cache => QuerySource::Cache,
            DetailQuerySource::Hosts => QuerySource::Hosts,
            DetailQuerySource::Rule => QuerySource::Rule,
            DetailQuerySource::Upstream => QuerySource::Upstream,
            DetailQuerySource::Synthetic => QuerySource::Synthetic,
        },
        outcome: match record.outcome {
            DetailQueryOutcome::Answered => QueryOutcome::Answered,
            DetailQueryOutcome::Negative => QueryOutcome::Negative,
            DetailQueryOutcome::Timeout => QueryOutcome::Timeout,
            DetailQueryOutcome::Rejected => QueryOutcome::Rejected,
            DetailQueryOutcome::Failed => QueryOutcome::Failed,
        },
        cache: match record.cache {
            DetailQueryCacheOutcome::Hit => CacheOutcome::Hit,
            DetailQueryCacheOutcome::Stale => CacheOutcome::Stale,
            DetailQueryCacheOutcome::Miss => CacheOutcome::Miss,
            DetailQueryCacheOutcome::Bypass => CacheOutcome::Bypass,
        },
        strategy_name: record.strategy_id,
        upstream_target_name,
        upstream_used_name,
        cache_producer,
        duration_us: Some(duration_us),
        dns_core_duration_us: record.dns_core_duration_micros,
        answers,
    })
}

fn parse_qtype(value: &str) -> Result<u16, ErrorCode> {
    let upper = value.to_ascii_uppercase();
    if let Some(value) = upper.strip_prefix("TYPE") {
        return value.parse().map_err(|_| ErrorCode::InvalidArgument);
    }
    RecordType::from_str(&upper)
        .map(u16::from)
        .map_err(|_| ErrorCode::InvalidArgument)
}

fn canonical_qname(value: &str) -> Result<String, ErrorCode> {
    let mut name = Name::from_ascii(value).map_err(|_| ErrorCode::InvalidArgument)?;
    name.set_fqdn(true);
    Ok(name.to_ascii().to_ascii_lowercase())
}

fn qtype_name(value: u16) -> String {
    let record_type = RecordType::from(value);
    if matches!(record_type, RecordType::Unknown(_)) {
        format!("TYPE{value}")
    } else {
        record_type.to_string()
    }
}

fn parse_rcode(value: &str) -> Result<DetailQueryRcode, ErrorCode> {
    match value.to_ascii_uppercase().as_str() {
        "NOERROR" => Ok(DetailQueryRcode::NoError),
        "FORMERR" => Ok(DetailQueryRcode::FormErr),
        "SERVFAIL" => Ok(DetailQueryRcode::ServFail),
        "NXDOMAIN" => Ok(DetailQueryRcode::NxDomain),
        "NOTIMP" => Ok(DetailQueryRcode::NotImp),
        "REFUSED" => Ok(DetailQueryRcode::Refused),
        "OTHER" => Ok(DetailQueryRcode::Other),
        _ => Err(ErrorCode::InvalidArgument),
    }
}

fn rcode_name(value: u8) -> String {
    match value {
        0 => "NOERROR".to_owned(),
        1 => "FORMERR".to_owned(),
        2 => "SERVFAIL".to_owned(),
        3 => "NXDOMAIN".to_owned(),
        4 => "NOTIMP".to_owned(),
        5 => "REFUSED".to_owned(),
        value => format!("RCODE{value}"),
    }
}

fn normalize_ip(value: IpAddr) -> IpAddr {
    match value {
        IpAddr::V6(value) => value.to_ipv4_mapped().map_or(IpAddr::V6(value), IpAddr::V4),
        value => value,
    }
}

fn storage_error(error: PortError) -> ErrorCode {
    if error.operation() == "detail_query.cursor" {
        ErrorCode::CursorExpired
    } else if matches!(error.class(), PortErrorClass::InvalidInput) {
        ErrorCode::InvalidArgument
    } else {
        ErrorCode::ServiceUnavailable
    }
}
