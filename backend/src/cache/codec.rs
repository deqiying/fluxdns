//! FDCS 快照复用的 canonical cache record codec；不包含独立持久化 adapter。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hickory_proto::op::{Message, MessageType, OpCode};

use crate::dns::{CanonicalQuery, CanonicalResponse, DnsMessageId, ResponseClass, RuntimeRevision};
use crate::ports::cache::{
    CACHE_ENTRY_FORMAT_VERSION, CacheEntry, CacheKey, CacheNamespace, CacheQuality, CacheRecord,
    CacheResponseClass, CacheUpstreamId, CacheUpstreamProvenance, CacheVersion,
};

use super::key::CACHE_KEY_FORMAT_VERSION;

const MAX_COMPONENT_BYTES: u32 = 64 * 1024;
const MAX_UPSTREAM_ID_BYTES: u32 = 128;
const MAX_DNS_WIRE_BYTES: u32 = 65_535;
#[derive(Debug)]
pub(super) enum CodecError {
    Corrupt,
    Incompatible,
    ResourceExhausted,
}

pub(super) fn encode_record(
    key: &CacheKey,
    record: &CacheRecord,
    now: Instant,
) -> Result<Vec<u8>, CodecError> {
    let wire = record
        .entry
        .response
        .as_message()
        .to_vec()
        .map_err(|_| CodecError::Corrupt)?;
    if wire.len() as u32 > MAX_DNS_WIRE_BYTES {
        return Err(CodecError::ResourceExhausted);
    }
    let mut output = Vec::with_capacity(256 + key.encoded.len() + wire.len());
    encode_namespace(&mut output, &key.namespace)?;
    put_u16(&mut output, key.format_version);
    put_bytes(&mut output, &key.encoded, MAX_COMPONENT_BYTES)?;
    put_u64(&mut output, record.version.0);
    put_u16(&mut output, record.entry.format_version);
    put_u64(&mut output, record.entry.producer_revision.0);
    output.push(encode_quality(record.entry.quality));
    output.push(encode_response_class(record.entry.response_class));
    put_u64(&mut output, record.entry.checksum);
    put_u64(
        &mut output,
        duration_nanos(now.saturating_duration_since(record.entry.inserted_at)),
    );
    put_u64(
        &mut output,
        unix_nanos_for_instant(record.entry.expires_at, now),
    );
    match record.entry.stale_until {
        Some(until) if until > now => {
            output.push(1);
            put_u64(&mut output, unix_nanos_for_instant(until, now));
        }
        _ => output.push(0),
    }
    put_bytes(
        &mut output,
        record.entry.upstream.target_id().as_str().as_bytes(),
        MAX_UPSTREAM_ID_BYTES,
    )?;
    match record.entry.upstream.used_id() {
        Some(used_id) => {
            output.push(1);
            put_bytes(
                &mut output,
                used_id.as_str().as_bytes(),
                MAX_UPSTREAM_ID_BYTES,
            )?;
        }
        None => output.push(0),
    }
    put_bytes(&mut output, &wire, MAX_DNS_WIRE_BYTES)?;
    Ok(output)
}

/// SQLite 主键复用 codec 的 namespace、格式与完整 key，不以摘要替代缓存身份。
pub(super) fn encode_storage_key(key: &CacheKey) -> Result<Vec<u8>, CodecError> {
    let mut output = Vec::new();
    encode_namespace(&mut output, &key.namespace)?;
    put_u16(&mut output, key.format_version);
    put_bytes(&mut output, &key.encoded, MAX_COMPONENT_BYTES)?;
    Ok(output)
}

pub(super) fn decode_record(
    payload: &[u8],
    now: Instant,
) -> Result<(CacheKey, CacheRecord), CodecError> {
    let mut reader = Reader::new(payload);
    let namespace = decode_namespace(&mut reader)?;
    let key_format = reader.u16()?;
    if key_format != CACHE_KEY_FORMAT_VERSION {
        return Err(CodecError::Incompatible);
    }
    let encoded = Arc::<[u8]>::from(reader.bytes(MAX_COMPONENT_BYTES)?.to_vec());
    let version = CacheVersion(reader.u64()?);
    let entry_format = reader.u16()?;
    if entry_format != 2 && entry_format != CACHE_ENTRY_FORMAT_VERSION {
        return Err(CodecError::Incompatible);
    }
    let producer_revision = RuntimeRevision(reader.u64()?);
    let quality = decode_quality(reader.byte()?)?;
    let response_class = decode_response_class(reader.byte()?)?;
    let checksum = reader.u64()?;
    let age = Duration::from_nanos(reader.u64()?);
    let expires_unix_nanos = reader.u64()?;
    let stale_until = match reader.byte()? {
        0 => None,
        1 => Some(instant_from_unix_nanos(reader.u64()?, now).ok_or(CodecError::Corrupt)?),
        _ => return Err(CodecError::Corrupt),
    };
    let target_id = decode_upstream_id(&mut reader)?;
    let used_id = match reader.byte()? {
        0 => None,
        1 => Some(decode_upstream_id(&mut reader)?),
        _ => return Err(CodecError::Corrupt),
    };
    let wire = reader.bytes(MAX_DNS_WIRE_BYTES)?.to_vec();
    if !reader.is_empty() {
        return Err(CodecError::Corrupt);
    }
    let message = Message::from_vec(&wire).map_err(|_| CodecError::Corrupt)?;
    if message.metadata.message_type != MessageType::Response || message.metadata.id != 0 {
        return Err(CodecError::Corrupt);
    }
    let mut query_message = Message::new(0, MessageType::Query, OpCode::Query);
    query_message.metadata.recursion_desired = message.metadata.recursion_desired;
    query_message.metadata.authentic_data = message.metadata.authentic_data;
    query_message.metadata.checking_disabled = message.metadata.checking_disabled;
    query_message.queries = message.queries.clone();
    query_message.edns = message.edns.clone();
    let query = CanonicalQuery::from_message(query_message).map_err(|_| CodecError::Corrupt)?;
    let response = CanonicalResponse::from_message(message, &query, DnsMessageId::new(0))
        .map_err(|_| CodecError::Corrupt)?;
    // v2 的空响应可能仍持有数小时的 fresh/stale 窗口，不能作为 v3 短缓存恢复。
    // 保留同布局的普通 v2 答案，避免升级时无关正缓存全部失效。
    if entry_format == 2 && response.short_cache_ttl().is_some() {
        return Err(CodecError::Incompatible);
    }
    if response_class != response_class_from_response(response.class())
        || checksum
            != super::admission::canonical_checksum(&response).map_err(|_| CodecError::Corrupt)?
    {
        return Err(CodecError::Corrupt);
    }
    let inserted_at = now.checked_sub(age).ok_or(CodecError::Corrupt)?;
    let expires_at = instant_from_unix_nanos(expires_unix_nanos, now).ok_or(CodecError::Corrupt)?;
    let key = CacheKey {
        namespace,
        encoded,
        format_version: key_format,
    };
    let stale_until = if response.short_cache_ttl().is_some() {
        None
    } else {
        stale_until
    };
    let entry = CacheEntry {
        response: Arc::new(response),
        upstream: CacheUpstreamProvenance::new(target_id, used_id),
        inserted_at,
        expires_at,
        stale_until,
        response_class,
        producer_revision,
        quality,
        checksum,
        format_version: CACHE_ENTRY_FORMAT_VERSION,
    };
    Ok((
        key,
        CacheRecord {
            version,
            entry: Arc::new(entry),
        },
    ))
}

fn decode_upstream_id(reader: &mut Reader<'_>) -> Result<CacheUpstreamId, CodecError> {
    let value = std::str::from_utf8(reader.bytes(MAX_UPSTREAM_ID_BYTES)?)
        .map_err(|_| CodecError::Corrupt)?;
    CacheUpstreamId::from_validated_config_id(value).map_err(|_| CodecError::Corrupt)
}

fn encode_namespace(output: &mut Vec<u8>, namespace: &CacheNamespace) -> Result<(), CodecError> {
    match namespace {
        CacheNamespace::Global => output.push(0),
        CacheNamespace::Strategy(strategy) => {
            output.push(1);
            put_bytes(output, strategy.as_bytes(), MAX_COMPONENT_BYTES)?;
        }
        CacheNamespace::ClientStrategy {
            client_digest,
            strategy,
        } => {
            output.push(2);
            output.extend_from_slice(&client_digest.as_bytes());
            put_bytes(output, strategy.as_bytes(), MAX_COMPONENT_BYTES)?;
        }
    }
    Ok(())
}

fn decode_namespace(reader: &mut Reader<'_>) -> Result<CacheNamespace, CodecError> {
    match reader.byte()? {
        0 => Ok(CacheNamespace::Global),
        1 => {
            let value = std::str::from_utf8(reader.bytes(MAX_COMPONENT_BYTES)?)
                .map_err(|_| CodecError::Corrupt)?;
            crate::ports::cache::CacheStrategyId::from_validated_config_id(value)
                .map(CacheNamespace::Strategy)
                .map_err(|_| CodecError::Corrupt)
        }
        2 => {
            let digest = reader.array_32()?;
            let value = std::str::from_utf8(reader.bytes(MAX_COMPONENT_BYTES)?)
                .map_err(|_| CodecError::Corrupt)?;
            let strategy = crate::ports::cache::CacheStrategyId::from_validated_config_id(value)
                .map_err(|_| CodecError::Corrupt)?;
            Ok(CacheNamespace::ClientStrategy {
                client_digest: crate::ports::cache::ClientCacheDigest::from_digest(digest),
                strategy,
            })
        }
        _ => Err(CodecError::Corrupt),
    }
}

fn response_class_from_response(class: ResponseClass) -> CacheResponseClass {
    match class {
        ResponseClass::Positive => CacheResponseClass::NoError,
        ResponseClass::NoData => CacheResponseClass::NoData,
        ResponseClass::NxDomain => CacheResponseClass::NxDomain,
        ResponseClass::ServFail => CacheResponseClass::ServFail,
        ResponseClass::Truncated => CacheResponseClass::Truncated,
        ResponseClass::Refused => CacheResponseClass::Refused,
        ResponseClass::Other(_) => CacheResponseClass::Other,
    }
}

fn encode_quality(quality: CacheQuality) -> u8 {
    quality as u8
}

fn decode_quality(value: u8) -> Result<CacheQuality, CodecError> {
    match value {
        0 => Ok(CacheQuality::Failure),
        1 => Ok(CacheQuality::Negative),
        2 => Ok(CacheQuality::Complete),
        _ => Err(CodecError::Corrupt),
    }
}

fn encode_response_class(class: CacheResponseClass) -> u8 {
    match class {
        CacheResponseClass::NoError => 0,
        CacheResponseClass::NoData => 1,
        CacheResponseClass::NxDomain => 2,
        CacheResponseClass::ServFail => 3,
        CacheResponseClass::Truncated => 4,
        CacheResponseClass::Refused => 5,
        CacheResponseClass::Other => 6,
    }
}

fn decode_response_class(value: u8) -> Result<CacheResponseClass, CodecError> {
    match value {
        0 => Ok(CacheResponseClass::NoError),
        1 => Ok(CacheResponseClass::NoData),
        2 => Ok(CacheResponseClass::NxDomain),
        3 => Ok(CacheResponseClass::ServFail),
        4 => Ok(CacheResponseClass::Truncated),
        5 => Ok(CacheResponseClass::Refused),
        6 => Ok(CacheResponseClass::Other),
        _ => Err(CodecError::Corrupt),
    }
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn system_time_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, duration_nanos)
}

fn unix_nanos_for_instant(at: Instant, reference: Instant) -> u64 {
    system_time_nanos().saturating_add(duration_nanos(at.saturating_duration_since(reference)))
}

fn instant_from_unix_nanos(value: u64, reference: Instant) -> Option<Instant> {
    let now_unix = system_time_nanos();
    if value <= now_unix {
        return Some(reference);
    }
    reference.checked_add(Duration::from_nanos(value - now_unix))
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_be_bytes());
}
fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}
fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8], maximum: u32) -> Result<(), CodecError> {
    let length = u32::try_from(value.len()).map_err(|_| CodecError::ResourceExhausted)?;
    if length > maximum {
        return Err(CodecError::ResourceExhausted);
    }
    put_u32(output, length);
    output.extend_from_slice(value);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], CodecError> {
        let end = self.offset.checked_add(length).ok_or(CodecError::Corrupt)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(CodecError::Corrupt)?;
        self.offset = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, CodecError> {
        self.take(1).map(|bytes| bytes[0])
    }
    fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn array_32(&mut self) -> Result<[u8; 32], CodecError> {
        self.take(32)?.try_into().map_err(|_| CodecError::Corrupt)
    }

    fn bytes(&mut self, maximum: u32) -> Result<&'a [u8], CodecError> {
        let length = self.u32()?;
        if length > maximum {
            return Err(CodecError::ResourceExhausted);
        }
        self.take(length as usize)
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheAdmissionOutcome, CacheAdmissionPolicy, admit_response};
    use hickory_proto::op::{Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A};

    fn record(code: ResponseCode, with_answer: bool) -> (CacheKey, CacheRecord) {
        let mut message = Message::new(0, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_ascii("snapshot.example.").unwrap(),
            RecordType::A,
        ));
        let query = CanonicalQuery::from_message(message).unwrap();
        let answers = with_answer.then(|| {
            Record::from_rdata(
                query.question().name().clone(),
                30,
                RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, 1))),
            )
        });
        let response = CanonicalResponse::response_with_code(&query, code, answers).unwrap();
        let CacheAdmissionOutcome::Accepted(entry) = admit_response(
            CacheAdmissionPolicy::default(),
            Arc::new(response),
            CacheUpstreamProvenance::direct_from_validated_config_id("remote").unwrap(),
            Instant::now() - Duration::from_secs(4),
            RuntimeRevision(1),
        )
        .unwrap() else {
            panic!("expected admission");
        };
        (
            CacheKey {
                namespace: CacheNamespace::Global,
                encoded: Arc::from([1_u8, 2]),
                format_version: CACHE_KEY_FORMAT_VERSION,
            },
            CacheRecord {
                version: CacheVersion(1),
                entry,
            },
        )
    }

    #[test]
    fn short_response_snapshot_preserves_code_class_and_remaining_lifetime() {
        for code in [
            ResponseCode::NoError,
            ResponseCode::NXDomain,
            ResponseCode::Refused,
            ResponseCode::ServFail,
            ResponseCode::FormErr,
            ResponseCode::NotImp,
        ] {
            let (key, record) = record(code, false);
            let payload = encode_record(&key, &record, Instant::now()).unwrap();
            let now = Instant::now();
            let (restored_key, restored) = decode_record(&payload, now).unwrap();
            assert_eq!(restored_key, key);
            assert_eq!(
                restored.entry.response.as_message().metadata.response_code,
                code
            );
            assert_eq!(restored.entry.response_class, record.entry.response_class);
            assert_eq!(restored.entry.quality, record.entry.quality);
            assert_eq!(restored.entry.checksum, record.entry.checksum);
            assert_eq!(restored.entry.stale_until, None);
            let remaining = restored.entry.expires_at.saturating_duration_since(now);
            assert!(remaining <= Duration::from_secs(6));
            assert!(remaining > Duration::from_secs(4));
        }
    }

    #[test]
    fn legacy_short_entries_are_cold_but_normal_answers_remain_compatible() {
        for with_answer in [false, true] {
            let (key, mut record) = record(ResponseCode::NoError, with_answer);
            Arc::get_mut(&mut record.entry).unwrap().format_version = 2;
            let payload = encode_record(&key, &record, Instant::now()).unwrap();
            let decoded = decode_record(&payload, Instant::now());
            if with_answer {
                let (_, restored) = decoded.unwrap();
                assert_eq!(restored.entry.format_version, CACHE_ENTRY_FORMAT_VERSION);
                assert_eq!(restored.entry.response, record.entry.response);
            } else {
                assert!(matches!(decoded, Err(CodecError::Incompatible)));
            }
        }
    }

    #[test]
    fn expired_short_snapshot_does_not_regain_fresh_or_stale_time() {
        let (key, mut record) = record(ResponseCode::Refused, false);
        let entry = Arc::get_mut(&mut record.entry).unwrap();
        entry.expires_at = Instant::now() - Duration::from_secs(1);
        entry.stale_until = Some(Instant::now() + Duration::from_secs(3600));
        let payload = encode_record(&key, &record, Instant::now()).unwrap();
        let now = Instant::now();
        let (_, restored) = decode_record(&payload, now).unwrap();
        assert!(restored.entry.expires_at <= now);
        assert_eq!(restored.entry.stale_until, None);
    }

    #[test]
    fn v2_keys_are_incompatible_even_when_the_entry_is_current() {
        let (mut key, record) = record(ResponseCode::NoError, true);
        key.format_version = 2;
        let payload = encode_record(&key, &record, Instant::now()).unwrap();
        assert!(matches!(
            decode_record(&payload, Instant::now()),
            Err(CodecError::Incompatible)
        ));
    }
}
