//! 无外部依赖的 cache persistence 快照 adapter。
//!
//! 该 adapter 固定了持久化 port 的边界和恢复语义；SQLite schema/writer 仍由后续阶段替换。

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hickory_proto::op::{Message, MessageType, OpCode};
use thiserror::Error;

use crate::dns::{CanonicalQuery, CanonicalResponse, DnsMessageId, ResponseClass, RuntimeRevision};
use crate::ports::cache::{
    CacheEntry, CacheKey, CacheNamespace, CacheQuality, CacheRecord, CacheRecoverySummary,
    CacheResponseClass, CacheVersion, PersistentCacheBatch, PersistentCacheStore,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

const MAGIC: &[u8; 4] = b"FDCP";
const FORMAT_VERSION: u16 = 1;
const MAX_RECORDS: u32 = 100_000;
const MAX_RECORD_BYTES: u32 = 2 * 1024 * 1024;
const MAX_COMPONENT_BYTES: u32 = 64 * 1024;
const MAX_DNS_WIRE_BYTES: u32 = 65_535;
const HEADER_BYTES: u64 = 10;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct FilePersistentCacheStore {
    path: Arc<PathBuf>,
    max_size_bytes: u64,
    state: Arc<Mutex<FileState>>,
}

#[derive(Default)]
struct FileState {
    records: HashMap<CacheKey, CacheRecord>,
    shutting_down: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum FilePersistentCacheStoreBuildError {
    #[error("persistent cache max size must be greater than zero")]
    ZeroMaxSize,
    #[error("persistent cache max size is smaller than the snapshot header")]
    MaxSizeTooSmall,
}

impl FilePersistentCacheStore {
    pub fn new(
        path: impl Into<PathBuf>,
        max_size_bytes: u64,
    ) -> Result<Self, FilePersistentCacheStoreBuildError> {
        if max_size_bytes == 0 {
            return Err(FilePersistentCacheStoreBuildError::ZeroMaxSize);
        }
        if max_size_bytes < HEADER_BYTES {
            return Err(FilePersistentCacheStoreBuildError::MaxSizeTooSmall);
        }
        Ok(Self {
            path: Arc::new(path.into()),
            max_size_bytes,
            state: Arc::new(Mutex::new(FileState::default())),
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub const fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }
}

impl std::fmt::Debug for FilePersistentCacheStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FilePersistentCacheStore")
            .field("path", &self.path)
            .field("max_size_bytes", &self.max_size_bytes)
            .finish_non_exhaustive()
    }
}

fn lock<'a>(
    mutex: &'a Mutex<FileState>,
    operation: &'static str,
) -> Result<MutexGuard<'a, FileState>, PortError> {
    mutex
        .lock()
        .map_err(|_| PortError::new(PortErrorClass::Internal, operation))
}

fn timeout(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Timeout, operation)
}

fn unavailable(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Unavailable, operation)
}

fn io_error(error: &std::io::Error, operation: &'static str) -> PortError {
    let class = match error.kind() {
        std::io::ErrorKind::PermissionDenied => PortErrorClass::PermissionDenied,
        std::io::ErrorKind::InvalidData => PortErrorClass::CorruptData,
        _ => PortErrorClass::Unavailable,
    };
    PortError::new(class, operation)
}

impl PersistentCacheStore for FilePersistentCacheStore {
    fn recover(
        &self,
        deadline: crate::dns::Deadline,
    ) -> PortFuture<'_, Result<(PersistentCacheBatch, CacheRecoverySummary), PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("file_cache.recover"));
            }
            let mut state = lock(&self.state, "file_cache.recover")?;
            if state.shutting_down {
                return Err(unavailable("file_cache.recover"));
            }
            let bytes = match fs::metadata(self.path.as_ref()) {
                Ok(metadata) if metadata.len() > self.max_size_bytes => {
                    return Err(PortError::new(
                        PortErrorClass::ResourceExhausted,
                        "file_cache.recover",
                    ));
                }
                Ok(_) => match fs::read(self.path.as_ref()) {
                    Ok(bytes) => bytes,
                    Err(error) => return Err(io_error(&error, "file_cache.recover")),
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(io_error(&error, "file_cache.recover")),
            };
            if bytes.len() as u64 > self.max_size_bytes {
                return Err(PortError::new(
                    PortErrorClass::ResourceExhausted,
                    "file_cache.recover",
                ));
            }
            let now = Instant::now();
            let (records, summary) = if bytes.is_empty() {
                (HashMap::new(), CacheRecoverySummary::default())
            } else {
                decode_snapshot(&bytes, now)
                    .map_err(|error| error.into_port_error("file_cache.recover"))?
            };
            state.records = records;
            let batch = PersistentCacheBatch {
                records: state
                    .records
                    .iter()
                    .map(|(key, record)| (key.clone(), record.clone()))
                    .collect(),
            };
            Ok((batch, summary))
        })
    }

    fn persist(
        &self,
        batch: PersistentCacheBatch,
        deadline: crate::dns::Deadline,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("file_cache.persist"));
            }
            let mut state = lock(&self.state, "file_cache.persist")?;
            if state.shutting_down {
                return Err(unavailable("file_cache.persist"));
            }
            let mut candidate = state.records.clone();
            candidate.extend(batch.records);
            let (kept, bytes) = prepare_snapshot(candidate, self.max_size_bytes, Instant::now())
                .map_err(|error| error.into_port_error("file_cache.persist"))?;
            write_snapshot(self.path.as_ref(), &bytes)
                .map_err(|error| io_error(&error, "file_cache.persist"))?;
            state.records = kept;
            Ok(())
        })
    }

    fn maintain_capacity(
        &self,
        deadline: crate::dns::Deadline,
    ) -> PortFuture<'_, Result<u64, PortError>> {
        Box::pin(async move {
            if deadline.is_expired(Instant::now()) {
                return Err(timeout("file_cache.maintain_capacity"));
            }
            let mut state = lock(&self.state, "file_cache.maintain_capacity")?;
            if state.shutting_down {
                return Err(unavailable("file_cache.maintain_capacity"));
            }
            let before = state.records.len() as u64;
            let (kept, bytes) =
                prepare_snapshot(state.records.clone(), self.max_size_bytes, Instant::now())
                    .map_err(|error| error.into_port_error("file_cache.maintain_capacity"))?;
            let removed = before.saturating_sub(kept.len() as u64);
            if removed > 0 || !self.path.as_ref().exists() {
                write_snapshot(self.path.as_ref(), &bytes)
                    .map_err(|error| io_error(&error, "file_cache.maintain_capacity"))?;
            }
            state.records = kept;
            Ok(removed)
        })
    }

    fn shutdown(&self, _deadline: crate::dns::Deadline) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let mut state = lock(&self.state, "file_cache.shutdown")?;
            state.shutting_down = true;
            state.records.clear();
            Ok(())
        })
    }
}

#[derive(Debug)]
enum CodecError {
    Corrupt,
    Incompatible,
    ResourceExhausted,
}

impl CodecError {
    fn into_port_error(self, operation: &'static str) -> PortError {
        let class = match self {
            Self::Corrupt => PortErrorClass::CorruptData,
            Self::Incompatible => PortErrorClass::Unavailable,
            Self::ResourceExhausted => PortErrorClass::ResourceExhausted,
        };
        PortError::new(class, operation)
    }
}

#[derive(Clone)]
struct EncodedRecord {
    key: CacheKey,
    record: CacheRecord,
    payload: Vec<u8>,
}

fn is_visible(record: &CacheRecord, now: Instant) -> bool {
    now < record.entry.expires_at || record.entry.stale_until.is_some_and(|until| now < until)
}

fn prepare_snapshot(
    records: HashMap<CacheKey, CacheRecord>,
    max_size_bytes: u64,
    now: Instant,
) -> Result<(HashMap<CacheKey, CacheRecord>, Vec<u8>), CodecError> {
    let mut encoded = Vec::with_capacity(records.len());
    for (key, record) in records {
        if !is_visible(&record, now) {
            continue;
        }
        let payload = encode_record(&key, &record, now)?;
        if payload.len() as u64 + HEADER_BYTES + 4 > max_size_bytes {
            return Err(CodecError::ResourceExhausted);
        }
        encoded.push(EncodedRecord {
            key,
            record,
            payload,
        });
    }
    while snapshot_size(&encoded) > max_size_bytes {
        let Some(index) = encoded
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                left.record
                    .entry
                    .inserted_at
                    .cmp(&right.record.entry.inserted_at)
                    .then_with(|| left.record.version.0.cmp(&right.record.version.0))
                    .then_with(|| left.key.encoded.as_ref().cmp(right.key.encoded.as_ref()))
            })
            .map(|(index, _)| index)
        else {
            break;
        };
        encoded.remove(index);
    }
    let bytes = encode_snapshot(&encoded)?;
    let kept = encoded
        .into_iter()
        .map(|item| (item.key, item.record))
        .collect();
    Ok((kept, bytes))
}

fn snapshot_size(records: &[EncodedRecord]) -> u64 {
    HEADER_BYTES.saturating_add(
        records
            .iter()
            .map(|record| 4_u64 + record.payload.len() as u64)
            .sum(),
    )
}

fn encode_snapshot(records: &[EncodedRecord]) -> Result<Vec<u8>, CodecError> {
    let count = u32::try_from(records.len()).map_err(|_| CodecError::ResourceExhausted)?;
    let mut output = Vec::with_capacity(snapshot_size(records) as usize);
    output.extend_from_slice(MAGIC);
    put_u16(&mut output, FORMAT_VERSION);
    put_u32(&mut output, count);
    for record in records {
        let length =
            u32::try_from(record.payload.len()).map_err(|_| CodecError::ResourceExhausted)?;
        put_u32(&mut output, length);
        output.extend_from_slice(&record.payload);
    }
    Ok(output)
}

fn write_snapshot(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn encode_record(
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
    put_bytes(&mut output, &wire, MAX_DNS_WIRE_BYTES)?;
    Ok(output)
}

fn decode_snapshot(
    bytes: &[u8],
    now: Instant,
) -> Result<(HashMap<CacheKey, CacheRecord>, CacheRecoverySummary), CodecError> {
    let mut reader = Reader::new(bytes);
    if reader.bytes_exact(4)? != MAGIC {
        return Err(CodecError::Corrupt);
    }
    if reader.u16()? != FORMAT_VERSION {
        return Err(CodecError::Incompatible);
    }
    let count = reader.u32()?;
    if count > MAX_RECORDS {
        return Err(CodecError::ResourceExhausted);
    }
    let mut summary = CacheRecoverySummary::default();
    let mut records = HashMap::new();
    for _ in 0..count {
        let length = reader.u32()?;
        if length > MAX_RECORD_BYTES {
            summary.corrupt = summary.corrupt.saturating_add(1);
            reader.skip_bounded(length as usize)?;
            continue;
        }
        let payload = reader.bytes_exact(length as usize)?.to_vec();
        match decode_record(&payload, now) {
            Ok((key, record)) => {
                if !is_visible(&record, now) {
                    summary.expired = summary.expired.saturating_add(1);
                } else {
                    summary.loaded = summary.loaded.saturating_add(1);
                    records.insert(key, record);
                }
            }
            Err(CodecError::Incompatible) => {
                summary.incompatible = summary.incompatible.saturating_add(1);
            }
            Err(_) => {
                summary.corrupt = summary.corrupt.saturating_add(1);
            }
        }
    }
    if !reader.is_empty() {
        return Err(CodecError::Corrupt);
    }
    Ok((records, summary))
}

fn decode_record(payload: &[u8], now: Instant) -> Result<(CacheKey, CacheRecord), CodecError> {
    let mut reader = Reader::new(payload);
    let namespace = decode_namespace(&mut reader)?;
    let key_format = reader.u16()?;
    if key_format != 1 {
        return Err(CodecError::Incompatible);
    }
    let encoded = Arc::<[u8]>::from(reader.bytes(MAX_COMPONENT_BYTES)?.to_vec());
    let version = CacheVersion(reader.u64()?);
    let entry_format = reader.u16()?;
    if entry_format != 1 {
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
    let entry = CacheEntry {
        response: Arc::new(response),
        inserted_at,
        expires_at,
        stale_until,
        response_class,
        producer_revision,
        quality,
        checksum,
        format_version: entry_format,
    };
    Ok((
        key,
        CacheRecord {
            version,
            entry: Arc::new(entry),
        },
    ))
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
        ResponseClass::Refused | ResponseClass::Other(_) => CacheResponseClass::ServFail,
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
    }
}

fn decode_response_class(value: u8) -> Result<CacheResponseClass, CodecError> {
    match value {
        0 => Ok(CacheResponseClass::NoError),
        1 => Ok(CacheResponseClass::NoData),
        2 => Ok(CacheResponseClass::NxDomain),
        3 => Ok(CacheResponseClass::ServFail),
        4 => Ok(CacheResponseClass::Truncated),
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

    fn bytes_exact(&mut self, length: usize) -> Result<&'a [u8], CodecError> {
        self.take(length)
    }

    fn skip_bounded(&mut self, length: usize) -> Result<(), CodecError> {
        self.take(length).map(|_| ())
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use crate::dns::{CanonicalQuery, CanonicalResponse, Deadline, DnsMessageId};
    use crate::ports::PortErrorClass;
    use crate::ports::cache::{
        CacheNamespace, CacheQuality, CacheResponseClass, PersistentCacheStore,
    };

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn deadline() -> crate::dns::Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn path() -> PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("fluxdns-cache-{}-{id}.bin", std::process::id()))
    }

    fn response() -> CanonicalResponse {
        let mut query_message = Message::new(0, MessageType::Query, OpCode::Query);
        query_message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        let query = CanonicalQuery::from_message(query_message.clone()).unwrap();
        let mut response_message = Message::response(0, OpCode::Query);
        response_message.add_query(query_message.queries[0].clone());
        CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(0)).unwrap()
    }

    fn record(key: &str, version: u64, expires_at: Instant) -> (CacheKey, CacheRecord) {
        let response = Arc::new(response());
        let checksum = super::super::admission::canonical_checksum(response.as_ref()).unwrap();
        let key = CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(key.as_bytes()),
            format_version: 1,
        };
        let entry = CacheEntry {
            response,
            inserted_at: Instant::now(),
            expires_at,
            stale_until: None,
            response_class: CacheResponseClass::NoData,
            producer_revision: RuntimeRevision(version),
            quality: CacheQuality::Negative,
            checksum,
            format_version: 1,
        };
        (
            key,
            CacheRecord {
                version: CacheVersion(version),
                entry: Arc::new(entry),
            },
        )
    }

    #[tokio::test]
    async fn persists_and_recovers_a_canonical_record() {
        let path = path();
        let store = FilePersistentCacheStore::new(&path, 1024 * 1024).unwrap();
        let item = record("roundtrip", 7, Instant::now() + Duration::from_secs(30));
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![item.clone()],
                },
                deadline(),
            )
            .await
            .unwrap();

        let restored = FilePersistentCacheStore::new(&path, 1024 * 1024).unwrap();
        let (batch, summary) = restored.recover(deadline()).await.unwrap();
        assert_eq!(summary.loaded, 1);
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].0, item.0);
        assert_eq!(batch.records[0].1.version, item.1.version);
        assert_eq!(batch.records[0].1.entry.checksum, item.1.entry.checksum);
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn recovery_discards_expired_records() {
        let path = path();
        let store = FilePersistentCacheStore::new(&path, 1024 * 1024).unwrap();
        let item = record("expired", 1, Instant::now() + Duration::from_millis(20));
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![item],
                },
                deadline(),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let restored = FilePersistentCacheStore::new(&path, 1024 * 1024).unwrap();
        let (batch, summary) = restored.recover(deadline()).await.unwrap();
        assert!(batch.records.is_empty());
        assert_eq!(summary.expired, 1);
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn capacity_maintenance_evicts_oldest_record() {
        let path = path();
        let first = record("first", 1, Instant::now() + Duration::from_secs(30));
        tokio::time::sleep(Duration::from_millis(1)).await;
        let second = record("second", 2, Instant::now() + Duration::from_secs(30));
        let now = Instant::now();
        let first_size = encode_record(&first.0, &first.1, now).unwrap().len() as u64;
        let second_size = encode_record(&second.0, &second.1, now).unwrap().len() as u64;
        let max_size = HEADER_BYTES + 4 + first_size.max(second_size);
        let store = FilePersistentCacheStore::new(&path, max_size).unwrap();
        store
            .persist(
                PersistentCacheBatch {
                    records: vec![first.clone(), second.clone()],
                },
                deadline(),
            )
            .await
            .unwrap();
        let restored = FilePersistentCacheStore::new(&path, 200).unwrap();
        let (batch, _) = restored.recover(deadline()).await.unwrap();
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].0, second.0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn corrupt_checksum_is_isolated_to_the_record() {
        let now = Instant::now();
        let first = record("first", 1, now + Duration::from_secs(30));
        let second = record("second", 2, now + Duration::from_secs(30));
        let first_payload = encode_record(&first.0, &first.1, now).unwrap();
        let second_payload = encode_record(&second.0, &second.1, now).unwrap();
        let mut bytes = encode_snapshot(&[
            EncodedRecord {
                key: first.0,
                record: first.1,
                payload: first_payload.clone(),
            },
            EncodedRecord {
                key: second.0,
                record: second.1,
                payload: second_payload,
            },
        ])
        .unwrap();
        let second_offset = HEADER_BYTES as usize + 4 + first_payload.len() + 4;
        bytes[second_offset + 40] ^= 0xFF;
        let (records, summary) = decode_snapshot(&bytes, now).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(summary.corrupt, 1);
    }

    #[test]
    fn rejects_too_small_limits() {
        assert_eq!(
            FilePersistentCacheStore::new("cache", 0).unwrap_err(),
            FilePersistentCacheStoreBuildError::ZeroMaxSize
        );
        assert_eq!(
            FilePersistentCacheStore::new("cache", HEADER_BYTES - 1).unwrap_err(),
            FilePersistentCacheStoreBuildError::MaxSizeTooSmall
        );
    }

    #[tokio::test]
    async fn recovery_rejects_a_file_larger_than_the_configured_budget() {
        let path = path();
        fs::write(&path, [0_u8; 11]).unwrap();
        let store = FilePersistentCacheStore::new(&path, HEADER_BYTES).unwrap();
        let error = store.recover(deadline()).await.unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::ResourceExhausted));
        let _ = fs::remove_file(path);
    }
}
