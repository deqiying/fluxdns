//! 回退夹具：d7c7792f4c8c44961e4d004c74a29b53e225552d 的 read model 与 map_row。
//! 函数体保持基线原样；只复用未改变的标量解码 helper，不读取 ecs_json。
#![allow(dead_code)]
use super::*;

#[derive(Clone, PartialEq)]
pub(super) struct DetailQueryRecord {
    pub listener_id: Option<String>,
    pub execution: Option<crate::dns::RequestTraceSnapshot>,
    pub id: DetailRecordId,
    pub occurred_at_millis: u64,
    pub duration_millis: u64,
    pub dns_core_duration_micros: Option<u64>,
    pub client_id: Option<String>,
    pub client_ip: Option<String>,
    pub client_match_source: Option<ClientMatchSource>,
    pub matched_client_id: Option<String>,
    pub qname: String,
    pub qtype: u16,
    pub transport: DetailQueryTransport,
    pub rcode: u8,
    pub source: DetailQuerySource,
    pub outcome: DetailQueryOutcome,
    pub cache: DetailQueryCacheOutcome,
    pub strategy_id: Option<String>,
    pub upstream_target_id: Option<String>,
    pub upstream_used_id: Option<String>,
    pub answers: Vec<ResolveAnswer>,
    pub answer_count: u32,
    pub answers_truncated: bool,
}
struct LocatedRecord {
    record: DetailQueryRecord,
    location: RecordLocation,
}

pub(super) fn read(
    row: &sqlx::sqlite::SqliteRow,
    day: i32,
) -> Result<DetailQueryRecord, PortError> {
    Ok(map_row(row, day)?.record)
}

fn map_row(row: &sqlx::sqlite::SqliteRow, day_utc: i32) -> Result<LocatedRecord, PortError> {
    let operation = "detail_query.row";
    // 旧只读分片尚无扩展列；不迁移历史文件，也不猜测发送/写入结果。
    let execution = match row.try_get::<Option<String>, _>("execution_json") {
        Ok(Some(json)) if json.len() <= 4096 => {
            Some(serde_json::from_str(&json).map_err(|_| corrupt(operation))?)
        }
        Ok(None) | Err(sqlx::Error::ColumnNotFound(_)) => None,
        _ => return Err(corrupt(operation)),
    };
    let listener_id = optional_text(row, "listener_id", operation)?;
    let row_id = positive_i64(row, "id", operation)?;
    let occurred_at_millis = nonnegative_u64(row, "event_time_utc_millis", operation)?;
    if utc_day(occurred_at_millis)? != day_utc {
        return Err(corrupt(operation));
    }
    let duration_millis = nonnegative_u64(row, "duration_millis", operation)?;
    let dns_core_duration_micros =
        optional_nonnegative_u64(row, "dns_core_duration_micros", operation)?;
    let client_id = optional_text(row, "client_id", operation)?;
    let client_ip = optional_text(row, "client_ip", operation)?
        .map(|value| {
            value
                .parse::<IpAddr>()
                .map(|parsed| parsed.to_string())
                .map_err(|_| corrupt(operation))
        })
        .transpose()?;
    let client_match_source = optional_text(row, "client_match_source", operation)?
        .map(|value| match value.as_str() {
            "id" => Ok(ClientMatchSource::Id),
            "ip" => Ok(ClientMatchSource::Ip),
            _ => Err(corrupt(operation)),
        })
        .transpose()?;
    let matched_client_id = optional_text(row, "matched_client_id", operation)?;
    if client_match_source.is_some() != matched_client_id.is_some() {
        return Err(corrupt(operation));
    }
    let qname = required_text(row, "canonical_qname", operation)?;
    let qtype =
        u16::try_from(nonnegative_u64(row, "qtype", operation)?).map_err(|_| corrupt(operation))?;
    let transport = match required_text(row, "transport", operation)?.as_str() {
        "udp" => DetailQueryTransport::Udp,
        "tcp" => DetailQueryTransport::Tcp,
        "doh" => DetailQueryTransport::Doh,
        _ => return Err(corrupt(operation)),
    };
    let rcode = u8::try_from(nonnegative_u64(row, "rcode", operation)?)
        .ok()
        .filter(|value| *value <= 15)
        .ok_or_else(|| corrupt(operation))?;
    let source = match required_text(row, "source", operation)?.as_str() {
        "cache" => DetailQuerySource::Cache,
        "hosts" => DetailQuerySource::Hosts,
        "rule_set" => DetailQuerySource::Rule,
        "upstream" => DetailQuerySource::Upstream,
        "synthetic" => DetailQuerySource::Synthetic,
        _ => return Err(corrupt(operation)),
    };
    let failure = optional_text(row, "failure_class", operation)?;
    let outcome = outcome_from_row(failure.as_deref(), rcode);
    let cache = match required_text(row, "cache_status", operation)?.as_str() {
        "fresh" => DetailQueryCacheOutcome::Hit,
        "stale" => DetailQueryCacheOutcome::Stale,
        "expired" => DetailQueryCacheOutcome::Expired,
        "miss" => DetailQueryCacheOutcome::Miss,
        "disabled" | "store_unavailable" | "write_rejected" => DetailQueryCacheOutcome::Bypass,
        _ => return Err(corrupt(operation)),
    };
    let strategy_id = optional_text(row, "strategy_id", operation)?;
    let upstream_target_id = optional_text(row, "upstream_id", operation)?;
    let upstream_used_id = optional_text(row, "upstream_used_id", operation)?;
    let answer_count = u32::try_from(nonnegative_u64(row, "answer_count", operation)?)
        .map_err(|_| corrupt(operation))?;
    let answers_truncated = match row
        .try_get::<i64, _>("answers_truncated")
        .map_err(|_| corrupt(operation))?
    {
        0 => false,
        1 => true,
        _ => return Err(corrupt(operation)),
    };
    let answer_json = required_text(row, "answer_summary_json", operation)?;
    if answer_json.len() > 4_096 {
        return Err(corrupt(operation));
    }
    let answers =
        serde_json::from_str::<Vec<ResolveAnswer>>(&answer_json).map_err(|_| corrupt(operation))?;
    let stored_count = u32::try_from(answers.len()).map_err(|_| corrupt(operation))?;
    if answers.len() > 16
        || answer_count < stored_count
        || (!answers_truncated && answer_count != stored_count)
    {
        return Err(corrupt(operation));
    }
    let location = RecordLocation { day_utc, row_id };
    Ok(LocatedRecord {
        record: DetailQueryRecord {
            listener_id,
            execution,
            id: DetailRecordId::from_location(day_utc, row_id),
            occurred_at_millis,
            duration_millis,
            dns_core_duration_micros,
            client_id,
            client_ip,
            client_match_source,
            matched_client_id,
            qname,
            qtype,
            transport,
            rcode,
            source,
            outcome,
            cache,
            strategy_id,
            upstream_target_id,
            upstream_used_id,
            answers,
            answer_count,
            answers_truncated,
        },
        location,
    })
}
