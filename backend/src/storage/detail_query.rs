//! 详情日分片的稳定记录 ID、keyset cursor、跨分片读取和提交后通知。

use std::cmp::Ordering;
use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Row, Sqlite};
use tokio::sync::broadcast;

use crate::dns::Deadline;
use crate::dns::TransportClass;
use crate::ports::observation::ClientMatchSource;
use crate::ports::storage::{ResolveAnswer, StatsSource};
use crate::ports::telemetry::{CacheStatus, OutcomeClass};
use crate::ports::{PortError, PortErrorClass};

use super::detail_shards::{DetailShardStore, format_shard_file_name};
use super::resolve_log::ResolveDetailRecord;

const MAX_QUERY_SPAN_MILLIS: u64 = 3_650 * 86_400_000;
const MAX_PAGE_SIZE: u16 = 100;
const MAX_MATCHED_CLIENT_IDS: usize = 1_024;
// 这里只负责把 commit 无等待移交给 Management replay owner。
const COMMIT_CHANNEL_CAPACITY: usize = 16;
const RECORD_ID_PREFIX: &str = "qry1_";
const CURSOR_PREFIX: &str = "cur1_";
const RECORD_ID_PAYLOAD_LEN: usize = 13;
const RECORD_ID_DIGEST_LEN: usize = 12;
const CURSOR_PAYLOAD_LEN: usize = 48;
const CURSOR_DIGEST_LEN: usize = 16;
const RECORD_ID_DOMAIN: &[u8] = b"fluxdns.detail.record.v1";
const CURSOR_DOMAIN: &[u8] = b"fluxdns.detail.cursor.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetailRecordId(String);

impl DetailRecordId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_location(day_utc: i32, row_id: i64) -> Self {
        debug_assert!(row_id > 0);
        debug_assert!(format_shard_file_name(day_utc).is_some());
        let mut payload = [0_u8; RECORD_ID_PAYLOAD_LEN];
        payload[0] = 1;
        payload[1..5].copy_from_slice(&day_utc.to_be_bytes());
        payload[5..13].copy_from_slice(&row_id.to_be_bytes());
        let digest = record_id_digest(&payload);
        let mut encoded = Vec::with_capacity(RECORD_ID_PAYLOAD_LEN + RECORD_ID_DIGEST_LEN);
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(&digest);
        Self(format!(
            "{RECORD_ID_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(encoded)
        ))
    }

    fn location(&self) -> Result<RecordLocation, PortError> {
        let encoded = self
            .0
            .strip_prefix(RECORD_ID_PREFIX)
            .ok_or_else(|| invalid_cursor("detail_query.record_id", "invalid record id"))?;
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| invalid_cursor("detail_query.record_id", "invalid record id"))?;
        if decoded.len() != RECORD_ID_PAYLOAD_LEN + RECORD_ID_DIGEST_LEN || decoded[0] != 1 {
            return Err(invalid_cursor(
                "detail_query.record_id",
                "invalid record id",
            ));
        }
        let (payload, supplied_digest) = decoded.split_at(RECORD_ID_PAYLOAD_LEN);
        if supplied_digest != record_id_digest(payload) {
            return Err(invalid_cursor(
                "detail_query.record_id",
                "invalid record id",
            ));
        }
        let day_utc = i32::from_be_bytes(payload[1..5].try_into().unwrap());
        let row_id = i64::from_be_bytes(payload[5..13].try_into().unwrap());
        if row_id <= 0 || format_shard_file_name(day_utc).is_none() {
            return Err(invalid_cursor(
                "detail_query.record_id",
                "invalid record id",
            ));
        }
        Ok(RecordLocation { day_utc, row_id })
    }
}

impl TryFrom<String> for DetailRecordId {
    type Error = PortError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let id = Self(value);
        id.location()?;
        Ok(id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailPageDirection {
    Older,
    Newer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailQuerySort {
    OccurredAt,
    Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailSortOrder {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailQueryTransport {
    Udp,
    Tcp,
    Doh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailQuerySource {
    Cache,
    Hosts,
    Rule,
    Upstream,
    Synthetic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailQueryOutcome {
    Answered,
    Negative,
    Timeout,
    Rejected,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailQueryCacheOutcome {
    Hit,
    Stale,
    Miss,
    Bypass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailQueryRcode {
    NoError,
    FormErr,
    ServFail,
    NxDomain,
    NotImp,
    Refused,
    Other,
}

/// Management 在进入 storage 前完成名称到 ID 集合的解析；storage 只按历史事实过滤。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DetailQueryFilter {
    pub from_utc_millis: u64,
    pub to_utc_millis: u64,
    pub client_id: Option<String>,
    pub client_ip: Option<String>,
    pub qname: Option<String>,
    pub transport: Option<DetailQueryTransport>,
    pub matched_client_id: Option<String>,
    pub matched_client_ids: Vec<String>,
    /// `client_name` 已在完整当前目录解析；即使结果为空也必须应用该过滤。
    pub require_matched_client_ids: bool,
    pub qtype: Option<u16>,
    pub rcode: Option<DetailQueryRcode>,
    pub source: Option<DetailQuerySource>,
    pub outcome: Option<DetailQueryOutcome>,
    pub cache: Option<DetailQueryCacheOutcome>,
}

impl DetailQueryFilter {
    /// WS replay 与 SQLite 查询共用同一已规范化过滤语义。
    pub fn matches_record(&self, record: &DetailQueryRecord) -> bool {
        record.occurred_at_millis >= self.from_utc_millis
            && record.occurred_at_millis < self.to_utc_millis
            && self
                .client_id
                .as_ref()
                .is_none_or(|value| record.client_id.as_ref() == Some(value))
            && self
                .client_ip
                .as_ref()
                .is_none_or(|value| record.client_ip.as_ref() == Some(value))
            && self
                .qname
                .as_ref()
                .is_none_or(|value| record.qname == *value)
            && self.transport.is_none_or(|value| record.transport == value)
            && self
                .matched_client_id
                .as_ref()
                .is_none_or(|value| record.matched_client_id.as_ref() == Some(value))
            && (!self.require_matched_client_ids
                || self
                    .matched_client_ids
                    .iter()
                    .any(|value| record.matched_client_id.as_ref() == Some(value)))
            && self.qtype.is_none_or(|value| record.qtype == value)
            && self
                .rcode
                .is_none_or(|value| rcode_matches(value, record.rcode))
            && self.source.is_none_or(|value| record.source == value)
            && self.outcome.is_none_or(|value| record.outcome == value)
            && self.cache.is_none_or(|value| record.cache == value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetailQuery {
    pub filter: DetailQueryFilter,
    pub cursor: Option<String>,
    pub direction: DetailPageDirection,
    pub page_size: u16,
    pub sort: DetailQuerySort,
    pub order: DetailSortOrder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetailCommitCursor {
    pub epoch: String,
    pub sequence: u64,
}

#[derive(Clone, PartialEq)]
pub struct DetailQueryRecord {
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

impl fmt::Debug for DetailQueryRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetailQueryRecord")
            .field("id", &self.id)
            .field("occurred_at_millis", &self.occurred_at_millis)
            .field("duration_millis", &self.duration_millis)
            .field("dns_core_duration_micros", &self.dns_core_duration_micros)
            .field("has_client_id", &self.client_id.is_some())
            .field("has_client_ip", &self.client_ip.is_some())
            .field("client_match_source", &self.client_match_source)
            .field("has_matched_client_id", &self.matched_client_id.is_some())
            .field("qname_byte_len", &self.qname.len())
            .field("qtype", &self.qtype)
            .field("transport", &self.transport)
            .field("rcode", &self.rcode)
            .field("source", &self.source)
            .field("outcome", &self.outcome)
            .field("cache", &self.cache)
            .field("has_strategy", &self.strategy_id.is_some())
            .field("has_upstream_target", &self.upstream_target_id.is_some())
            .field("has_upstream_used", &self.upstream_used_id.is_some())
            .field("stored_answer_count", &self.answers.len())
            .field("answer_count", &self.answer_count)
            .field("answers_truncated", &self.answers_truncated)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DetailQueryPage {
    pub items: Vec<DetailQueryRecord>,
    pub previous_cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub snapshot_cursor: DetailCommitCursor,
    pub retention_revision: u64,
    pub available_from_utc_millis: Option<u64>,
}

#[derive(Clone)]
pub struct DetailCommittedRecord {
    pub id: DetailRecordId,
    pub record: ResolveDetailRecord,
}

impl DetailCommittedRecord {
    /// 将刚提交的内存记录投影为与 SQLite 读取完全相同的 Management read model。
    pub fn to_query_record(&self) -> Result<DetailQueryRecord, PortError> {
        let record = &self.record;
        let occurred_at_millis = record
            .occurred_at()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|value| u64::try_from(value.as_millis()).ok())
            .ok_or_else(|| corrupt("detail_query.committed_record"))?;
        let source = match record.source() {
            StatsSource::Cache => DetailQuerySource::Cache,
            StatsSource::Hosts => DetailQuerySource::Hosts,
            StatsSource::RuleSet => DetailQuerySource::Rule,
            StatsSource::Upstream => DetailQuerySource::Upstream,
        };
        let outcome = match record.outcome() {
            OutcomeClass::Timeout => DetailQueryOutcome::Timeout,
            OutcomeClass::Failure | OutcomeClass::Cancelled | OutcomeClass::Dropped => {
                DetailQueryOutcome::Failed
            }
            OutcomeClass::Success | OutcomeClass::Rejected => {
                outcome_from_row(None, record.rcode())
            }
        };
        let cache = match record.cache_status() {
            CacheStatus::Fresh => DetailQueryCacheOutcome::Hit,
            CacheStatus::Stale => DetailQueryCacheOutcome::Stale,
            CacheStatus::Miss => DetailQueryCacheOutcome::Miss,
            CacheStatus::Disabled | CacheStatus::StoreUnavailable | CacheStatus::WriteRejected => {
                DetailQueryCacheOutcome::Bypass
            }
        };
        Ok(DetailQueryRecord {
            id: self.id.clone(),
            occurred_at_millis,
            duration_millis: record.duration_millis(),
            dns_core_duration_micros: Some(record.dns_core_duration_micros()),
            client_id: record.client_id().map(str::to_owned),
            client_ip: record
                .client_ip()
                .map(|value| normalize_ip(value).to_string()),
            client_match_source: record.client_match_source(),
            matched_client_id: record.matched_client_id().map(str::to_owned),
            qname: record.qname().to_owned(),
            qtype: record.qtype(),
            transport: match record.transport() {
                TransportClass::Datagram => DetailQueryTransport::Udp,
                TransportClass::Stream => DetailQueryTransport::Tcp,
                TransportClass::Multiplexed => DetailQueryTransport::Doh,
            },
            rcode: record.rcode(),
            source,
            outcome,
            cache,
            strategy_id: record.strategy_id().map(str::to_owned),
            upstream_target_id: record.upstream_id().map(str::to_owned),
            upstream_used_id: record.upstream_used_id().map(str::to_owned),
            answers: record.answers().to_vec(),
            answer_count: record.answer_count(),
            answers_truncated: record.answers_truncated(),
        })
    }

    /// replay 字节预算按可见字段的 UTF-8 长度保守估算，不序列化敏感 Debug。
    pub fn estimated_replay_bytes(&self) -> usize {
        let record = &self.record;
        let optional = [
            record.client_id(),
            record.matched_client_id(),
            record.strategy_id(),
            record.upstream_id(),
            record.upstream_used_id(),
        ]
        .into_iter()
        .flatten()
        .map(json_string_budget)
        .sum::<usize>();
        512_usize
            .saturating_add(json_string_budget(self.id.as_str()))
            .saturating_add(json_string_budget(record.qname()))
            .saturating_add(optional)
            .saturating_add(
                record
                    .answers()
                    .iter()
                    .map(|answer| {
                        json_string_budget(&answer.name)
                            + json_string_budget(&answer.record_type)
                            + json_string_budget(&answer.data)
                            + 64
                    })
                    .sum(),
            )
    }
}

/// JSON 控制字符最多展开为六字节 `\u00xx` 转义，replay 预算按该上界计数。
fn json_string_budget(value: &str) -> usize {
    value.len().saturating_mul(6)
}

impl fmt::Debug for DetailCommittedRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetailCommittedRecord")
            .field("id", &self.id)
            .field("record", &self.record)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct DetailCommitNotification {
    pub cursor: DetailCommitCursor,
    pub records: Arc<[DetailCommittedRecord]>,
}

struct CommitClock {
    epoch: [u8; 16],
    sequence: u64,
}

pub(super) struct DetailQueryState {
    cursor_key: [u8; 32],
    commit_clock: Mutex<CommitClock>,
    retention_revision: AtomicU64,
    commits: broadcast::Sender<Arc<DetailCommitNotification>>,
}

impl DetailQueryState {
    pub(super) fn new() -> Result<Self, super::detail_shards::DetailShardStoreBuildError> {
        let mut cursor_key = [0_u8; 32];
        let mut epoch = [0_u8; 16];
        getrandom::fill(&mut cursor_key)
            .map_err(|_| super::detail_shards::DetailShardStoreBuildError::Entropy)?;
        getrandom::fill(&mut epoch)
            .map_err(|_| super::detail_shards::DetailShardStoreBuildError::Entropy)?;
        let (commits, _) = broadcast::channel(COMMIT_CHANNEL_CAPACITY);
        Ok(Self {
            cursor_key,
            commit_clock: Mutex::new(CommitClock { epoch, sequence: 0 }),
            retention_revision: AtomicU64::new(0),
            commits,
        })
    }

    pub(super) fn advance_retention_revision(&self) {
        advance_revision(&self.retention_revision);
    }

    pub(super) fn publish_commit(
        &self,
        day_utc: i32,
        records: &[ResolveDetailRecord],
        row_ids: &[i64],
    ) {
        debug_assert_eq!(records.len(), row_ids.len());
        let cursor = {
            let mut clock = self.commit_clock.lock().unwrap();
            if clock.sequence == u64::MAX {
                let digest = Sha256::digest(clock.epoch);
                clock.epoch.copy_from_slice(&digest[..16]);
                clock.sequence = 1;
            } else {
                clock.sequence += 1;
            }
            commit_cursor(&clock)
        };
        let committed = records
            .iter()
            .cloned()
            .zip(row_ids.iter().copied())
            .map(|(record, row_id)| DetailCommittedRecord {
                id: DetailRecordId::from_location(day_utc, row_id),
                record,
            })
            .collect::<Vec<_>>();
        debug_assert_eq!(committed.len(), records.len());
        let _ = self.commits.send(Arc::new(DetailCommitNotification {
            cursor,
            records: committed.into(),
        }));
    }

    fn snapshot_cursor(&self) -> DetailCommitCursor {
        commit_cursor(&self.commit_clock.lock().unwrap())
    }
}

impl DetailShardStore {
    /// 订阅详情事务提交事件；接收滞后处理与 replay 属于 BC-25。
    pub fn subscribe_commits(&self) -> broadcast::Receiver<Arc<DetailCommitNotification>> {
        self.query_state.commits.subscribe()
    }

    /// 捕获当前 commit 边界，供 HTTP 快照与后续 replay 交接。
    pub fn detail_commit_cursor(&self) -> DetailCommitCursor {
        self.query_state.snapshot_cursor()
    }

    /// 返回与 HTTP cursor 同源的共同保留水位 revision，供实时订阅检测失效。
    pub fn detail_retention_revision(&self) -> u64 {
        self.query_state
            .retention_revision
            .load(AtomicOrdering::Acquire)
    }

    /// 在所有可见日分片内执行有界 keyset 查询。
    pub async fn query_details(
        &self,
        query: DetailQuery,
        deadline: Deadline,
    ) -> Result<DetailQueryPage, PortError> {
        validate_query(&query)?;
        let fingerprint = filter_fingerprint(&query)?;
        let retention_revision = self
            .query_state
            .retention_revision
            .load(AtomicOrdering::Acquire);
        let anchor = query
            .cursor
            .as_deref()
            .map(|cursor| self.decode_cursor(cursor, &query, fingerprint, retention_revision))
            .transpose()?;
        if query.cursor.is_none() && query.direction == DetailPageDirection::Newer {
            return Err(invalid_cursor(
                "detail_query.cursor",
                "newer direction requires a cursor",
            ));
        }

        let snapshot_cursor = self.detail_commit_cursor();
        let from_day = utc_day(query.filter.from_utc_millis)?;
        let to_day = utc_day(query.filter.to_utc_millis - 1)?;
        let per_shard_limit = usize::from(query.page_size) + 1;
        let mut candidates = Vec::with_capacity(per_shard_limit);
        for day_utc in from_day..=to_day {
            check_deadline(deadline)?;
            let Some(lease) = self.acquire_read(day_utc, deadline).await? else {
                continue;
            };
            let shard = query_shard(
                lease.pool(),
                day_utc,
                &query,
                anchor,
                per_shard_limit,
                deadline,
            )
            .await;
            let close = lease.close(deadline).await;
            let rows = match (shard, close) {
                (Ok(rows), Ok(())) => rows,
                (Err(error), _) | (Ok(_), Err(error)) => return Err(error),
            };
            for row in rows {
                insert_bounded(&mut candidates, row, &query, per_shard_limit);
            }
        }
        candidates.sort_by(|left, right| compare_scan(left, right, &query));
        let has_more = candidates.len() > usize::from(query.page_size);
        candidates.truncate(usize::from(query.page_size));
        if query.direction == DetailPageDirection::Newer {
            candidates.reverse();
        }

        let current_retention_revision = self
            .query_state
            .retention_revision
            .load(AtomicOrdering::Acquire);
        if current_retention_revision != retention_revision {
            return Err(invalid_cursor(
                "detail_query.cursor",
                "retention changed during query",
            ));
        }
        let previous_cursor = match (candidates.first(), query.direction) {
            (Some(record), DetailPageDirection::Older) if query.cursor.is_some() => {
                Some(self.encode_cursor(
                    record,
                    &query,
                    DetailPageDirection::Newer,
                    fingerprint,
                    retention_revision,
                ))
            }
            (Some(record), DetailPageDirection::Newer) if has_more => Some(self.encode_cursor(
                record,
                &query,
                DetailPageDirection::Newer,
                fingerprint,
                retention_revision,
            )),
            _ => None,
        };
        let next_cursor = match (candidates.last(), query.direction) {
            (Some(record), DetailPageDirection::Older) if has_more => Some(self.encode_cursor(
                record,
                &query,
                DetailPageDirection::Older,
                fingerprint,
                retention_revision,
            )),
            (Some(record), DetailPageDirection::Newer) if query.cursor.is_some() => {
                Some(self.encode_cursor(
                    record,
                    &query,
                    DetailPageDirection::Older,
                    fingerprint,
                    retention_revision,
                ))
            }
            _ => None,
        };
        let available_from_utc_millis = self.available_from_utc_millis()?;
        Ok(DetailQueryPage {
            items: candidates.into_iter().map(|value| value.record).collect(),
            previous_cursor,
            next_cursor,
            snapshot_cursor,
            retention_revision,
            available_from_utc_millis,
        })
    }

    /// 由稳定记录 ID 定位单一日分片；缺失和逻辑退役均返回 `None`。
    pub async fn read_detail(
        &self,
        id: &DetailRecordId,
        deadline: Deadline,
    ) -> Result<Option<DetailQueryRecord>, PortError> {
        let location = id.location()?;
        let Some(lease) = self.acquire_read(location.day_utc, deadline).await? else {
            return Ok(None);
        };
        let row = deadline_future(
            deadline,
            "detail_query.read",
            sqlx::query(SELECT_COLUMNS)
                .bind(location.row_id)
                .fetch_optional(lease.pool()),
        )
        .await?
        .map_err(|_| unavailable("detail_query.read"))?;
        let result = row
            .map(|row| map_row(&row, location.day_utc))
            .transpose()?
            .map(|value| value.record);
        lease.close(deadline).await?;
        Ok(result)
    }

    fn available_from_utc_millis(&self) -> Result<Option<u64>, PortError> {
        let retired_before = self.retired_before();
        let entries = match std::fs::read_dir(self.root()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(unavailable("detail_query.directory")),
        };
        let first = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(super::detail_shards::parse_shard_file_name)
            })
            .filter(|day| retired_before.is_none_or(|watermark| *day >= watermark))
            .min();
        first
            .map(|day| {
                u64::try_from(day)
                    .ok()
                    .and_then(|value| value.checked_mul(86_400_000))
                    .ok_or_else(|| corrupt("detail_query.directory"))
            })
            .transpose()
    }

    fn encode_cursor(
        &self,
        anchor: &LocatedRecord,
        query: &DetailQuery,
        direction: DetailPageDirection,
        fingerprint: [u8; 16],
        retention_revision: u64,
    ) -> String {
        let mut payload = [0_u8; CURSOR_PAYLOAD_LEN];
        payload[0] = 1;
        payload[1..17].copy_from_slice(&fingerprint);
        payload[17..25].copy_from_slice(&retention_revision.to_be_bytes());
        payload[25] = sort_byte(query.sort);
        payload[26] = order_byte(query.order);
        payload[27] = direction_byte(direction);
        payload[28..36].copy_from_slice(&anchor.primary(query.sort).to_be_bytes());
        payload[36..40].copy_from_slice(&anchor.location.day_utc.to_be_bytes());
        payload[40..48].copy_from_slice(&anchor.location.row_id.to_be_bytes());
        let digest = cursor_digest(&self.query_state.cursor_key, &payload);
        let mut encoded = Vec::with_capacity(CURSOR_PAYLOAD_LEN + CURSOR_DIGEST_LEN);
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(&digest);
        format!("{CURSOR_PREFIX}{}", URL_SAFE_NO_PAD.encode(encoded))
    }

    fn decode_cursor(
        &self,
        cursor: &str,
        query: &DetailQuery,
        fingerprint: [u8; 16],
        retention_revision: u64,
    ) -> Result<CursorAnchor, PortError> {
        let encoded = cursor
            .strip_prefix(CURSOR_PREFIX)
            .ok_or_else(|| invalid_cursor("detail_query.cursor", "invalid cursor"))?;
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| invalid_cursor("detail_query.cursor", "invalid cursor"))?;
        if decoded.len() != CURSOR_PAYLOAD_LEN + CURSOR_DIGEST_LEN || decoded[0] != 1 {
            return Err(invalid_cursor("detail_query.cursor", "invalid cursor"));
        }
        let (payload, supplied_digest) = decoded.split_at(CURSOR_PAYLOAD_LEN);
        if supplied_digest != cursor_digest(&self.query_state.cursor_key, payload)
            || payload[1..17] != fingerprint
            || payload[25] != sort_byte(query.sort)
            || payload[26] != order_byte(query.order)
            || payload[27] != direction_byte(query.direction)
        {
            return Err(invalid_cursor(
                "detail_query.cursor",
                "cursor context mismatch",
            ));
        }
        let cursor_retention = u64::from_be_bytes(payload[17..25].try_into().unwrap());
        if cursor_retention != retention_revision {
            return Err(invalid_cursor(
                "detail_query.cursor",
                "cursor retention revision expired",
            ));
        }
        let primary = u64::from_be_bytes(payload[28..36].try_into().unwrap());
        let day_utc = i32::from_be_bytes(payload[36..40].try_into().unwrap());
        let row_id = i64::from_be_bytes(payload[40..48].try_into().unwrap());
        if row_id <= 0 || format_shard_file_name(day_utc).is_none() {
            return Err(invalid_cursor(
                "detail_query.cursor",
                "invalid cursor anchor",
            ));
        }
        Ok(CursorAnchor {
            primary,
            location: RecordLocation { day_utc, row_id },
        })
    }
}

const SELECT_COLUMNS: &str = "SELECT id, event_time_utc_millis, duration_millis, \
    dns_core_duration_micros, client_id, client_ip, client_match_source, matched_client_id, \
    canonical_qname, qtype, transport, rcode, source, failure_class, cache_status, strategy_id, \
    upstream_id, upstream_used_id, answer_count, answers_truncated, answer_summary_json \
    FROM resolve_log WHERE id = ?";

const QUERY_COLUMNS: &str = "SELECT id, event_time_utc_millis, duration_millis, \
    dns_core_duration_micros, client_id, client_ip, client_match_source, matched_client_id, \
    canonical_qname, qtype, transport, rcode, source, failure_class, cache_status, strategy_id, \
    upstream_id, upstream_used_id, answer_count, answers_truncated, answer_summary_json \
    FROM resolve_log WHERE event_time_utc_millis >= ";

#[derive(Clone, Copy)]
struct RecordLocation {
    day_utc: i32,
    row_id: i64,
}

#[derive(Clone, Copy)]
struct CursorAnchor {
    primary: u64,
    location: RecordLocation,
}

struct LocatedRecord {
    record: DetailQueryRecord,
    location: RecordLocation,
}

impl LocatedRecord {
    fn primary(&self, sort: DetailQuerySort) -> u64 {
        match sort {
            DetailQuerySort::OccurredAt => self.record.occurred_at_millis,
            DetailQuerySort::Duration => self.record.duration_millis,
        }
    }
}

async fn query_shard(
    pool: &sqlx::SqlitePool,
    day_utc: i32,
    query: &DetailQuery,
    anchor: Option<CursorAnchor>,
    limit: usize,
    deadline: Deadline,
) -> Result<Vec<LocatedRecord>, PortError> {
    let from =
        i64::try_from(query.filter.from_utc_millis).map_err(|_| invalid("detail_query.range"))?;
    let to =
        i64::try_from(query.filter.to_utc_millis).map_err(|_| invalid("detail_query.range"))?;
    let mut sql = QueryBuilder::<Sqlite>::new(QUERY_COLUMNS);
    sql.push_bind(from)
        .push(" AND event_time_utc_millis < ")
        .push_bind(to);
    if let Some(value) = &query.filter.client_id {
        sql.push(" AND client_id = ").push_bind(value);
    }
    if let Some(value) = &query.filter.client_ip {
        sql.push(" AND client_ip = ").push_bind(value);
    }
    if let Some(value) = &query.filter.qname {
        sql.push(" AND canonical_qname = ").push_bind(value);
    }
    if let Some(value) = query.filter.transport {
        sql.push(" AND transport = ")
            .push_bind(transport_name(value));
    }
    if let Some(value) = &query.filter.matched_client_id {
        sql.push(" AND matched_client_id = ").push_bind(value);
    }
    if query.filter.require_matched_client_ids && query.filter.matched_client_ids.is_empty() {
        sql.push(" AND 1 = 0");
    } else if !query.filter.matched_client_ids.is_empty() {
        sql.push(" AND matched_client_id IN (");
        let mut values = sql.separated(", ");
        for value in &query.filter.matched_client_ids {
            values.push_bind(value);
        }
        values.push_unseparated(")");
    }
    if let Some(value) = query.filter.qtype {
        sql.push(" AND qtype = ").push_bind(i64::from(value));
    }
    if let Some(value) = query.filter.rcode {
        match value {
            DetailQueryRcode::Other => {
                sql.push(" AND rcode NOT IN (0, 1, 2, 3, 4, 5)");
            }
            _ => {
                sql.push(" AND rcode = ")
                    .push_bind(i64::from(rcode_value(value)));
            }
        }
    }
    if let Some(value) = query.filter.source {
        sql.push(" AND source = ").push_bind(source_name(value));
    }
    if let Some(value) = query.filter.outcome {
        push_outcome_filter(&mut sql, value);
    }
    if let Some(value) = query.filter.cache {
        push_cache_filter(&mut sql, value);
    }

    let primary_column = match query.sort {
        DetailQuerySort::OccurredAt => "event_time_utc_millis",
        DetailQuerySort::Duration => "duration_millis",
    };
    let scan_ascending = scan_ascending(query);
    if let Some(anchor) = anchor {
        let comparison = if scan_ascending { ">" } else { "<" };
        sql.push(" AND (")
            .push(primary_column)
            .push(" ")
            .push(comparison)
            .push(" ")
            .push_bind(i64::try_from(anchor.primary).map_err(|_| invalid("detail_query.cursor"))?)
            .push(" OR (")
            .push(primary_column)
            .push(" = ")
            .push_bind(i64::try_from(anchor.primary).map_err(|_| invalid("detail_query.cursor"))?)
            .push(" AND ");
        match day_utc.cmp(&anchor.location.day_utc) {
            Ordering::Greater => {
                sql.push(if scan_ascending { "1 = 1" } else { "1 = 0" });
            }
            Ordering::Less => {
                sql.push(if scan_ascending { "1 = 0" } else { "1 = 1" });
            }
            Ordering::Equal => {
                sql.push("id ")
                    .push(comparison)
                    .push(" ")
                    .push_bind(anchor.location.row_id);
            }
        }
        sql.push("))");
    }
    let order = if scan_ascending { " ASC" } else { " DESC" };
    sql.push(" ORDER BY ")
        .push(primary_column)
        .push(order)
        .push(", id")
        .push(order)
        .push(" LIMIT ")
        .push_bind(i64::try_from(limit).map_err(|_| invalid("detail_query.limit"))?);
    let rows = deadline_future(deadline, "detail_query.search", sql.build().fetch_all(pool))
        .await?
        .map_err(|_| unavailable("detail_query.search"))?;
    rows.iter().map(|row| map_row(row, day_utc)).collect()
}

fn map_row(row: &sqlx::sqlite::SqliteRow, day_utc: i32) -> Result<LocatedRecord, PortError> {
    let operation = "detail_query.row";
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

fn validate_query(query: &DetailQuery) -> Result<(), PortError> {
    let filter = &query.filter;
    if query.page_size == 0
        || query.page_size > MAX_PAGE_SIZE
        || filter.from_utc_millis >= filter.to_utc_millis
        || filter.to_utc_millis > i64::MAX as u64
        || filter.to_utc_millis - filter.from_utc_millis > MAX_QUERY_SPAN_MILLIS
        || filter.matched_client_ids.len() > MAX_MATCHED_CLIENT_IDS
        || filter
            .client_ip
            .as_ref()
            .is_some_and(|value| value.parse::<IpAddr>().is_err())
        || filter
            .qname
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 1_024)
        || filter
            .client_id
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 128)
        || filter
            .matched_client_id
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 128)
        || filter
            .matched_client_ids
            .iter()
            .any(|value| value.is_empty() || value.len() > 128)
    {
        return Err(invalid("detail_query.validate"));
    }
    utc_day(filter.from_utc_millis)?;
    utc_day(filter.to_utc_millis - 1)?;
    Ok(())
}

fn filter_fingerprint(query: &DetailQuery) -> Result<[u8; 16], PortError> {
    let mut filter = query.filter.clone();
    filter.matched_client_ids.sort();
    filter.matched_client_ids.dedup();
    let encoded = serde_json::to_vec(&(filter, query.sort, query.order))
        .map_err(|_| invalid("detail_query.cursor"))?;
    let digest = Sha256::digest(encoded);
    Ok(digest[..16].try_into().unwrap())
}

fn push_outcome_filter(sql: &mut QueryBuilder<Sqlite>, value: DetailQueryOutcome) {
    match value {
        DetailQueryOutcome::Answered => {
            sql.push(" AND failure_class IS NULL AND rcode NOT IN (1, 2, 3, 4, 5)");
        }
        DetailQueryOutcome::Negative => {
            sql.push(" AND failure_class IS NULL AND rcode = 3");
        }
        DetailQueryOutcome::Timeout => {
            sql.push(" AND failure_class = 'timeout'");
        }
        DetailQueryOutcome::Rejected => {
            sql.push(" AND failure_class IS NULL AND rcode IN (1, 4, 5)");
        }
        DetailQueryOutcome::Failed => {
            sql.push(" AND ((failure_class IS NOT NULL AND failure_class != 'timeout') OR (failure_class IS NULL AND rcode = 2))");
        }
    }
}

fn push_cache_filter(sql: &mut QueryBuilder<Sqlite>, value: DetailQueryCacheOutcome) {
    match value {
        DetailQueryCacheOutcome::Hit => {
            sql.push(" AND cache_status = 'fresh'");
        }
        DetailQueryCacheOutcome::Stale => {
            sql.push(" AND cache_status = 'stale'");
        }
        DetailQueryCacheOutcome::Miss => {
            sql.push(" AND cache_status = 'miss'");
        }
        DetailQueryCacheOutcome::Bypass => {
            sql.push(" AND cache_status IN ('disabled', 'store_unavailable', 'write_rejected')");
        }
    }
}

fn insert_bounded(
    candidates: &mut Vec<LocatedRecord>,
    record: LocatedRecord,
    query: &DetailQuery,
    limit: usize,
) {
    candidates.push(record);
    candidates.sort_by(|left, right| compare_scan(left, right, query));
    if candidates.len() > limit {
        let _ = candidates.pop();
    }
}

fn compare_scan(left: &LocatedRecord, right: &LocatedRecord, query: &DetailQuery) -> Ordering {
    let order = left
        .primary(query.sort)
        .cmp(&right.primary(query.sort))
        .then_with(|| left.location.day_utc.cmp(&right.location.day_utc))
        .then_with(|| left.location.row_id.cmp(&right.location.row_id));
    if scan_ascending(query) {
        order
    } else {
        order.reverse()
    }
}

fn scan_ascending(query: &DetailQuery) -> bool {
    let base_ascending = query.order == DetailSortOrder::Asc;
    match query.direction {
        DetailPageDirection::Older => base_ascending,
        DetailPageDirection::Newer => !base_ascending,
    }
}

fn transport_name(value: DetailQueryTransport) -> &'static str {
    match value {
        DetailQueryTransport::Udp => "udp",
        DetailQueryTransport::Tcp => "tcp",
        DetailQueryTransport::Doh => "doh",
    }
}

fn source_name(value: DetailQuerySource) -> &'static str {
    match value {
        DetailQuerySource::Cache => "cache",
        DetailQuerySource::Hosts => "hosts",
        DetailQuerySource::Rule => "rule_set",
        DetailQuerySource::Upstream => "upstream",
        DetailQuerySource::Synthetic => "synthetic",
    }
}

fn rcode_value(value: DetailQueryRcode) -> u8 {
    match value {
        DetailQueryRcode::NoError => 0,
        DetailQueryRcode::FormErr => 1,
        DetailQueryRcode::ServFail => 2,
        DetailQueryRcode::NxDomain => 3,
        DetailQueryRcode::NotImp => 4,
        DetailQueryRcode::Refused => 5,
        DetailQueryRcode::Other => unreachable!(),
    }
}

fn outcome_from_row(failure: Option<&str>, rcode: u8) -> DetailQueryOutcome {
    match (failure, rcode) {
        (Some("timeout"), _) => DetailQueryOutcome::Timeout,
        (Some(_), _) | (None, 2) => DetailQueryOutcome::Failed,
        (None, 3) => DetailQueryOutcome::Negative,
        (None, 1 | 4 | 5) => DetailQueryOutcome::Rejected,
        (None, _) => DetailQueryOutcome::Answered,
    }
}

fn rcode_matches(expected: DetailQueryRcode, value: u8) -> bool {
    match expected {
        DetailQueryRcode::NoError => value == 0,
        DetailQueryRcode::FormErr => value == 1,
        DetailQueryRcode::ServFail => value == 2,
        DetailQueryRcode::NxDomain => value == 3,
        DetailQueryRcode::NotImp => value == 4,
        DetailQueryRcode::Refused => value == 5,
        DetailQueryRcode::Other => value > 5,
    }
}

fn normalize_ip(value: IpAddr) -> IpAddr {
    match value {
        IpAddr::V6(value) => value.to_ipv4_mapped().map_or(IpAddr::V6(value), IpAddr::V4),
        value => value,
    }
}

fn sort_byte(value: DetailQuerySort) -> u8 {
    match value {
        DetailQuerySort::OccurredAt => 0,
        DetailQuerySort::Duration => 1,
    }
}

fn order_byte(value: DetailSortOrder) -> u8 {
    match value {
        DetailSortOrder::Asc => 0,
        DetailSortOrder::Desc => 1,
    }
}

fn direction_byte(value: DetailPageDirection) -> u8 {
    match value {
        DetailPageDirection::Older => 0,
        DetailPageDirection::Newer => 1,
    }
}

fn record_id_digest(payload: &[u8]) -> [u8; RECORD_ID_DIGEST_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(RECORD_ID_DOMAIN);
    hasher.update(payload);
    hasher.finalize()[..RECORD_ID_DIGEST_LEN]
        .try_into()
        .unwrap()
}

fn cursor_digest(key: &[u8; 32], payload: &[u8]) -> [u8; CURSOR_DIGEST_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(CURSOR_DOMAIN);
    hasher.update(key);
    hasher.update(payload);
    hasher.update(key);
    hasher.finalize()[..CURSOR_DIGEST_LEN].try_into().unwrap()
}

fn commit_cursor(clock: &CommitClock) -> DetailCommitCursor {
    DetailCommitCursor {
        epoch: format!("dqs_{}", URL_SAFE_NO_PAD.encode(clock.epoch)),
        sequence: clock.sequence,
    }
}

fn advance_revision(revision: &AtomicU64) {
    let _ = revision.fetch_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |value| {
        Some(value.saturating_add(1))
    });
}

fn utc_day(millis: u64) -> Result<i32, PortError> {
    let day = i32::try_from(millis / 86_400_000).map_err(|_| invalid("detail_query.range"))?;
    if format_shard_file_name(day).is_none() {
        return Err(invalid("detail_query.range"));
    }
    Ok(day)
}

fn positive_i64(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
    operation: &'static str,
) -> Result<i64, PortError> {
    row.try_get::<i64, _>(column)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| corrupt(operation))
}

fn nonnegative_u64(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
    operation: &'static str,
) -> Result<u64, PortError> {
    row.try_get::<i64, _>(column)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| corrupt(operation))
}

fn optional_nonnegative_u64(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
    operation: &'static str,
) -> Result<Option<u64>, PortError> {
    row.try_get::<Option<i64>, _>(column)
        .map_err(|_| corrupt(operation))?
        .map(|value| u64::try_from(value).map_err(|_| corrupt(operation)))
        .transpose()
}

fn required_text(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
    operation: &'static str,
) -> Result<String, PortError> {
    optional_text(row, column, operation)?.ok_or_else(|| corrupt(operation))
}

fn optional_text(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
    operation: &'static str,
) -> Result<Option<String>, PortError> {
    let value = row
        .try_get::<Option<String>, _>(column)
        .map_err(|_| corrupt(operation))?;
    if value
        .as_deref()
        .is_some_and(|value| value.is_empty() || value == "<present>" || value == "<absent>")
    {
        return Err(corrupt(operation));
    }
    Ok(value)
}

fn check_deadline(deadline: Deadline) -> Result<(), PortError> {
    if deadline.is_expired(Instant::now()) {
        Err(PortError::new(
            PortErrorClass::Timeout,
            "detail_query.search",
        ))
    } else {
        Ok(())
    }
}

async fn deadline_future<F, T>(
    deadline: Deadline,
    operation: &'static str,
    future: F,
) -> Result<T, PortError>
where
    F: std::future::Future<Output = T>,
{
    check_deadline(deadline)?;
    tokio::time::timeout(deadline.remaining(Instant::now()), future)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Timeout, operation))
}

fn invalid(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::InvalidInput, operation)
}

fn invalid_cursor(operation: &'static str, context: &'static str) -> PortError {
    PortError::new(PortErrorClass::InvalidInput, operation).with_safe_context(context)
}

fn corrupt(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::CorruptData, operation)
}

fn unavailable(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Unavailable, operation)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use tokio::sync::broadcast::error::TryRecvError;

    use crate::dns::{Deadline, RuntimeRevision, TransportClass};
    use crate::ports::PortErrorClass;
    use crate::ports::observation::ClientMatchSource;
    use crate::ports::storage::{ResolveEvent, StatsSource};
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::ports::testing::TestGate;
    use crate::storage::ResolveDetailRecord;
    use crate::storage::detail_shards::{DetailShardStore, DetailSqlTestStage};

    use super::{
        DetailPageDirection, DetailQuery, DetailQueryCacheOutcome, DetailQueryFilter,
        DetailQueryOutcome, DetailQueryRcode, DetailQuerySort, DetailQuerySource,
        DetailQueryTransport, DetailRecordId, DetailSortOrder,
    };

    const DAY_MILLIS: u64 = 86_400_000;
    const FIRST_DAY: i32 = 20_704;
    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns")
            .join("p2-detail-query-tests")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn record(
        day_utc: i32,
        millis: u64,
        duration_millis: u64,
        qname: &str,
        client_id: &str,
        matched_client_id: &str,
    ) -> ResolveDetailRecord {
        let occurred_at = UNIX_EPOCH
            + Duration::from_millis(u64::try_from(day_utc).unwrap() * DAY_MILLIS + millis);
        ResolveDetailRecord::from_event(ResolveEvent {
            occurred_at,
            duration_millis,
            dns_core_duration_micros: duration_millis * 500,
            request_digest: Arc::from(format!("digest-{day_utc}-{millis}-{qname}")),
            listener_id: Arc::from("udp-main"),
            route_id: None,
            client_id: Some(Arc::from(client_id)),
            client_ip: Some("192.0.2.10".parse().unwrap()),
            client_match_source: Some(ClientMatchSource::Id),
            matched_client_id: Some(Arc::from(matched_client_id)),
            client_bucket: Some(Arc::from(matched_client_id)),
            strategy_id: Some(Arc::from("default")),
            upstream_id: Some(Arc::from("public")),
            upstream_member_id: None,
            upstream_used_id: Some(Arc::from("alidns")),
            matched_rule_source: None,
            matched_resource_id: None,
            matched_rule_ordinal: None,
            resource_version: None,
            transport: TransportClass::Datagram,
            qname: Arc::from(qname),
            qtype: 1,
            qclass: 1,
            answers: Vec::new(),
            rcode: 0,
            cancellation_reason: None,
            outcome: OutcomeClass::Success,
            source: StatsSource::Upstream,
            cache_status: CacheStatus::Miss,
            runtime_revision: RuntimeRevision(1),
        })
        .unwrap()
    }

    fn query(from_day: i32, to_day: i32, page_size: u16) -> DetailQuery {
        DetailQuery {
            filter: DetailQueryFilter {
                from_utc_millis: u64::try_from(from_day).unwrap() * DAY_MILLIS,
                to_utc_millis: u64::try_from(to_day + 1).unwrap() * DAY_MILLIS,
                ..DetailQueryFilter::default()
            },
            cursor: None,
            direction: DetailPageDirection::Older,
            page_size,
            sort: DetailQuerySort::OccurredAt,
            order: DetailSortOrder::Asc,
        }
    }

    #[test]
    fn stable_record_id_rejects_modified_or_unbounded_locations() {
        let id = DetailRecordId::from_location(FIRST_DAY, 42);
        assert_eq!(id.location().unwrap().day_utc, FIRST_DAY);
        assert_eq!(id.location().unwrap().row_id, 42);

        let mut modified = id.as_str().as_bytes().to_vec();
        let last = modified.last_mut().unwrap();
        *last = if *last == b'A' { b'B' } else { b'A' };
        let error = DetailRecordId::try_from(String::from_utf8(modified).unwrap()).unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::InvalidInput));
        assert!(DetailRecordId::try_from("../2026-09-08.sqlite3".to_owned()).is_err());
    }

    #[tokio::test]
    async fn real_sqlite_query_pages_across_days_and_reverses_without_gaps() {
        let root = test_root("pagination");
        let store = DetailShardStore::new(root.clone(), Vec::new(), 2).unwrap();
        store
            .write_records(
                FIRST_DAY,
                &[
                    record(FIRST_DAY, 1_000, 9, "a.example.", "raw-a", "client-a"),
                    record(FIRST_DAY, 1_000, 3, "b.example.", "raw-b", "client-b"),
                ],
                deadline(),
            )
            .await
            .unwrap();
        store
            .write_records(
                FIRST_DAY + 1,
                &[
                    record(FIRST_DAY + 1, 10, 7, "c.example.", "raw-c", "client-c"),
                    record(FIRST_DAY + 1, 20, 1, "d.example.", "raw-d", "client-d"),
                ],
                deadline(),
            )
            .await
            .unwrap();

        let first = store
            .query_details(query(FIRST_DAY, FIRST_DAY + 1, 2), deadline())
            .await
            .unwrap();
        assert_eq!(
            first
                .items
                .iter()
                .map(|item| item.qname.as_str())
                .collect::<Vec<_>>(),
            ["a.example.", "b.example."]
        );
        assert_ne!(first.items[0].id, first.items[1].id);
        assert!(first.previous_cursor.is_none());
        assert!(first.next_cursor.is_some());
        assert_eq!(first.snapshot_cursor.sequence, 2);
        assert_eq!(
            first.available_from_utc_millis,
            Some(u64::try_from(FIRST_DAY).unwrap() * DAY_MILLIS)
        );

        let mut next_query = query(FIRST_DAY, FIRST_DAY + 1, 2);
        next_query.cursor = first.next_cursor.clone();
        let second = store.query_details(next_query, deadline()).await.unwrap();
        assert_eq!(
            second
                .items
                .iter()
                .map(|item| item.qname.as_str())
                .collect::<Vec<_>>(),
            ["c.example.", "d.example."]
        );
        assert!(second.previous_cursor.is_some());
        assert!(second.next_cursor.is_none());

        let mut previous_query = query(FIRST_DAY, FIRST_DAY + 1, 2);
        previous_query.direction = DetailPageDirection::Newer;
        previous_query.cursor = second.previous_cursor;
        let previous = store
            .query_details(previous_query, deadline())
            .await
            .unwrap();
        assert_eq!(
            previous
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            first
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>()
        );
        assert!(previous.previous_cursor.is_none());
        assert!(previous.next_cursor.is_some());

        let selected = store
            .read_detail(&first.items[0].id, deadline())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.qname, "a.example.");
        let stable_ids = first
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        let old_cursor = first.next_cursor.unwrap();
        store.shutdown(deadline()).await.unwrap();
        drop(store);

        let reopened = DetailShardStore::new(root.clone(), Vec::new(), 2).unwrap();
        let reopened_page = reopened
            .query_details(query(FIRST_DAY, FIRST_DAY + 1, 2), deadline())
            .await
            .unwrap();
        assert_eq!(
            reopened_page
                .items
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            stable_ids
        );
        let mut stale = query(FIRST_DAY, FIRST_DAY + 1, 2);
        stale.cursor = Some(old_cursor);
        assert!(reopened.query_details(stale, deadline()).await.is_err());
        reopened.shutdown(deadline()).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn real_sqlite_filters_before_paging_and_merges_duration_order() {
        let root = test_root("filters");
        let store = DetailShardStore::new(root.clone(), Vec::new(), 2).unwrap();
        store
            .write_records(
                FIRST_DAY,
                &[
                    record(FIRST_DAY, 1, 8, "skip.example.", "raw-a", "client-a"),
                    record(FIRST_DAY, 2, 2, "target.example.", "raw-b", "client-b"),
                ],
                deadline(),
            )
            .await
            .unwrap();
        store
            .write_records(
                FIRST_DAY + 1,
                &[
                    record(FIRST_DAY + 1, 1, 6, "target.example.", "raw-b", "client-b"),
                    record(FIRST_DAY + 1, 2, 4, "skip.example.", "raw-b", "client-b"),
                ],
                deadline(),
            )
            .await
            .unwrap();

        let mut filtered = query(FIRST_DAY, FIRST_DAY + 1, 1);
        filtered.filter.client_id = Some("raw-b".to_owned());
        filtered.filter.client_ip = Some("192.0.2.10".to_owned());
        filtered.filter.qname = Some("target.example.".to_owned());
        filtered.filter.transport = Some(DetailQueryTransport::Udp);
        filtered.filter.matched_client_ids = vec!["client-b".to_owned(), "missing".to_owned()];
        filtered.filter.require_matched_client_ids = true;
        filtered.filter.qtype = Some(1);
        filtered.filter.rcode = Some(DetailQueryRcode::NoError);
        filtered.filter.source = Some(DetailQuerySource::Upstream);
        filtered.filter.outcome = Some(DetailQueryOutcome::Answered);
        filtered.filter.cache = Some(DetailQueryCacheOutcome::Miss);
        filtered.sort = DetailQuerySort::Duration;
        filtered.order = DetailSortOrder::Desc;
        let first = store
            .query_details(filtered.clone(), deadline())
            .await
            .unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].duration_millis, 6);
        assert!(first.next_cursor.is_some());

        filtered.cursor = first.next_cursor;
        let second = store.query_details(filtered, deadline()).await.unwrap();
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].duration_millis, 2);
        assert!(second.next_cursor.is_none());

        let mut no_current_name_match = query(FIRST_DAY, FIRST_DAY + 1, 20);
        no_current_name_match.filter.require_matched_client_ids = true;
        assert!(
            store
                .query_details(no_current_name_match, deadline())
                .await
                .unwrap()
                .items
                .is_empty()
        );
        store.shutdown(deadline()).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cursor_binds_filter_direction_watermark_and_integrity() {
        let root = test_root("cursor");
        let store = DetailShardStore::new(root.clone(), Vec::new(), 1).unwrap();
        store
            .write_records(
                FIRST_DAY,
                &[
                    record(FIRST_DAY, 1, 1, "a.example.", "raw", "client"),
                    record(FIRST_DAY, 2, 2, "b.example.", "raw", "client"),
                ],
                deadline(),
            )
            .await
            .unwrap();
        let page = store
            .query_details(query(FIRST_DAY, FIRST_DAY, 1), deadline())
            .await
            .unwrap();
        let cursor = page.next_cursor.unwrap();

        let mut wrong_filter = query(FIRST_DAY, FIRST_DAY, 1);
        wrong_filter.filter.qname = Some("b.example.".to_owned());
        wrong_filter.cursor = Some(cursor.clone());
        assert!(matches!(
            store
                .query_details(wrong_filter, deadline())
                .await
                .unwrap_err()
                .class(),
            PortErrorClass::InvalidInput
        ));

        let mut wrong_direction = query(FIRST_DAY, FIRST_DAY, 1);
        wrong_direction.direction = DetailPageDirection::Newer;
        wrong_direction.cursor = Some(cursor.clone());
        assert!(
            store
                .query_details(wrong_direction, deadline())
                .await
                .is_err()
        );

        let mut bytes = cursor.as_bytes().to_vec();
        let last = bytes.last_mut().unwrap();
        *last = if *last == b'A' { b'B' } else { b'A' };
        let mut modified = query(FIRST_DAY, FIRST_DAY, 1);
        modified.cursor = Some(String::from_utf8(bytes).unwrap());
        assert!(store.query_details(modified, deadline()).await.is_err());

        store.publish_retired_before(FIRST_DAY);
        let mut expired = query(FIRST_DAY, FIRST_DAY, 1);
        expired.cursor = Some(cursor);
        assert!(store.query_details(expired, deadline()).await.is_err());
        store.shutdown(deadline()).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn commit_notification_is_published_only_after_successful_commit() {
        let root = test_root("notifications");
        let store = Arc::new(DetailShardStore::new(root.clone(), Vec::new(), 2).unwrap());
        let mut receiver = store.subscribe_commits();
        let gate = Arc::new(TestGate::new());
        store.set_detail_test_gate(DetailSqlTestStage::BeforeCommit, Arc::clone(&gate));
        let write_store = Arc::clone(&store);
        let write = tokio::spawn(async move {
            write_store
                .write_records(
                    FIRST_DAY + 1,
                    &[record(
                        FIRST_DAY + 1,
                        10,
                        1,
                        "new.example.",
                        "raw",
                        "client",
                    )],
                    deadline(),
                )
                .await
        });
        gate.wait_reached().await;
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        gate.release();
        write.await.unwrap().unwrap();
        let first = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.cursor.sequence, 1);
        assert_eq!(first.records.len(), 1);

        store
            .write_records(
                FIRST_DAY,
                &[record(FIRST_DAY, 10, 1, "late.example.", "raw", "client")],
                deadline(),
            )
            .await
            .unwrap();
        let late = receiver.recv().await.unwrap();
        assert_eq!(late.cursor.sequence, 2);
        assert_eq!(late.cursor.epoch, first.cursor.epoch);
        assert!(late.records[0].record.occurred_at() < first.records[0].record.occurred_at());

        let failed = store
            .write_records(
                FIRST_DAY,
                &[record(
                    FIRST_DAY + 1,
                    20,
                    1,
                    "wrong-day.example.",
                    "raw",
                    "client",
                )],
                deadline(),
            )
            .await;
        assert!(failed.is_err());
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(store.detail_commit_cursor().sequence, 2);
        store.shutdown(deadline()).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn empty_read_is_non_creating_and_expired_deadline_is_reported() {
        let root = test_root("empty");
        let store = DetailShardStore::new(root.clone(), Vec::new(), 1).unwrap();
        let page = store
            .query_details(query(FIRST_DAY, FIRST_DAY, 20), deadline())
            .await
            .unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.snapshot_cursor.sequence, 0);
        assert!(!root.exists());

        let expired = Deadline::new(Instant::now() - Duration::from_millis(1));
        let error = store
            .query_details(query(FIRST_DAY, FIRST_DAY, 20), expired)
            .await
            .unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::Timeout));
        assert!(!root.exists());
    }

    #[test]
    fn query_record_debug_does_not_expose_request_values() {
        let record = super::DetailQueryRecord {
            id: DetailRecordId::from_location(FIRST_DAY, 1),
            occurred_at_millis: 1,
            duration_millis: 2,
            dns_core_duration_micros: Some(3),
            client_id: Some("secret-client".to_owned()),
            client_ip: Some("192.0.2.10".to_owned()),
            client_match_source: Some(ClientMatchSource::Id),
            matched_client_id: Some("matched-secret".to_owned()),
            qname: "secret.example.".to_owned(),
            qtype: 1,
            transport: DetailQueryTransport::Udp,
            rcode: 0,
            source: DetailQuerySource::Upstream,
            outcome: DetailQueryOutcome::Answered,
            cache: DetailQueryCacheOutcome::Miss,
            strategy_id: Some("strategy-secret".to_owned()),
            upstream_target_id: Some("upstream-secret".to_owned()),
            upstream_used_id: None,
            answers: Vec::new(),
            answer_count: 0,
            answers_truncated: false,
        };
        let debug = format!("{record:?}");
        for forbidden in [
            "secret-client",
            "192.0.2.10",
            "matched-secret",
            "secret.example.",
            "strategy-secret",
            "upstream-secret",
        ] {
            assert!(!debug.contains(forbidden), "{forbidden}");
        }
    }
}
