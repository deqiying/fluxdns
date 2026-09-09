//! 进程级缓存完整快照的流式文件协议。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::dns::Deadline;
use crate::ports::cache::{CacheKey, CacheRecord, CacheRecoverySummary};

use super::codec::{CodecError, decode_record, encode_record, encode_storage_key};
use super::moka::{MokaCacheStore, SnapshotVisitError};

const MAGIC: &[u8; 4] = b"FDCS";
const FORMAT_VERSION: u16 = 1;
const HEADER_BYTES: u64 = 60;
const MAX_RECORDS: u32 = 100_000;
const MAX_RECORD_BYTES: u32 = 2 * 1024 * 1024;
const CHECKSUM_BYTES: usize = 32;
const VERIFY_BUFFER_BYTES: usize = 64 * 1024;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheSnapshotWriteSummary {
    pub generated_at_utc_millis: u64,
    pub records: u32,
    pub bytes: u64,
}

#[derive(Debug, Error)]
pub enum CacheSnapshotError {
    #[error("cache snapshot operation timed out")]
    Timeout,
    #[error("cache snapshot source is unavailable")]
    Unavailable,
    #[error("cache snapshot generation was superseded")]
    Superseded,
    #[error("cache snapshot is corrupt")]
    Corrupt,
    #[error("cache snapshot format is incompatible")]
    Incompatible,
    #[error("cache snapshot exceeds an internal resource bound")]
    ResourceExhausted,
    #[error("cache snapshot I/O failed")]
    Io(#[source] std::io::Error),
}

impl From<std::io::Error> for CacheSnapshotError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// 将 Moka 当前可见记录分批编码到同目录临时文件，并完整发布为单一快照。
pub fn write_cache_snapshot(
    store: &MokaCacheStore,
    path: &Path,
    batch_size: usize,
    deadline: Deadline,
) -> Result<CacheSnapshotWriteSummary, CacheSnapshotError> {
    write_cache_snapshot_if_current(store, path, batch_size, deadline, || Ok(true))
}

/// 完成临时文件后在调用方 generation 仲裁下决定是否发布。
pub(crate) fn write_cache_snapshot_if_current(
    store: &MokaCacheStore,
    path: &Path,
    batch_size: usize,
    deadline: Deadline,
    publish_current: impl FnOnce() -> Result<bool, CacheSnapshotError>,
) -> Result<CacheSnapshotWriteSummary, CacheSnapshotError> {
    ensure_deadline(deadline)?;
    let parent = path.parent().filter(|value| !value.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    reject_non_regular_target(path)?;

    let temp_path = temporary_path(path);
    let result =
        write_temporary_snapshot(store, &temp_path, batch_size, deadline).and_then(|summary| {
            ensure_deadline(deadline)?;
            if !publish_current()? {
                return Err(CacheSnapshotError::Superseded);
            }
            replace_file(&temp_path, path)?;
            if let Some(parent) = parent {
                sync_directory(parent)?;
            }
            Ok(summary)
        });
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_temporary_snapshot(
    store: &MokaCacheStore,
    temp_path: &Path,
    batch_size: usize,
    deadline: Deadline,
) -> Result<CacheSnapshotWriteSummary, CacheSnapshotError> {
    let generated_at_utc_millis = system_time_millis();
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(temp_path)?;
    file.write_all(&[0_u8; HEADER_BYTES as usize])?;

    let now = Instant::now();
    let mut hasher = Sha256::new();
    let mut body_bytes = 0_u64;
    let mut record_count = 0_u32;
    let mut seen = HashSet::<[u8; CHECKSUM_BYTES]>::new();
    let visit = store.visit_snapshot_batches(batch_size, deadline, |batch| {
        for (key, record) in batch {
            ensure_deadline(deadline)?;
            let storage_key = encode_storage_key(&key).map_err(map_codec_error)?;
            let digest: [u8; CHECKSUM_BYTES] = Sha256::digest(storage_key).into();
            if !seen.insert(digest) {
                continue;
            }
            record_count = record_count
                .checked_add(1)
                .filter(|count| *count <= MAX_RECORDS)
                .ok_or(CacheSnapshotError::ResourceExhausted)?;
            let payload = encode_record(&key, &record, now).map_err(map_codec_error)?;
            let length = u32::try_from(payload.len())
                .ok()
                .filter(|length| *length <= MAX_RECORD_BYTES)
                .ok_or(CacheSnapshotError::ResourceExhausted)?;
            let length_bytes = length.to_be_bytes();
            file.write_all(&length_bytes)?;
            file.write_all(&payload)?;
            hasher.update(length_bytes);
            hasher.update(&payload);
            body_bytes = body_bytes
                .checked_add(4 + u64::from(length))
                .ok_or(CacheSnapshotError::ResourceExhausted)?;
        }
        Ok::<_, CacheSnapshotError>(())
    });
    match visit {
        Ok(_) => {}
        Err(SnapshotVisitError::ZeroBatchSize) => {
            return Err(CacheSnapshotError::ResourceExhausted);
        }
        Err(SnapshotVisitError::Timeout) => return Err(CacheSnapshotError::Timeout),
        Err(SnapshotVisitError::Unavailable) => return Err(CacheSnapshotError::Unavailable),
        Err(SnapshotVisitError::Visitor(error)) => return Err(error),
    }

    let header_without_checksum = Header {
        generated_at_utc_millis,
        record_count,
        body_bytes,
        checksum: [0; CHECKSUM_BYTES],
    };
    update_metadata_checksum(&mut hasher, header_without_checksum);
    let checksum: [u8; CHECKSUM_BYTES] = hasher.finalize().into();
    let header = encode_header(Header {
        checksum,
        ..header_without_checksum
    });
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    file.sync_all()?;
    drop(file);
    Ok(CacheSnapshotWriteSummary {
        generated_at_utc_millis,
        records: record_count,
        bytes: HEADER_BYTES + body_bytes,
    })
}

/// 已通过完整文件长度与 SHA-256 校验的有界流式恢复 reader。
pub struct CacheSnapshotReader {
    file: File,
    generated_at_utc_millis: u64,
    snapshot_bytes: u64,
    remaining: u32,
    now: Instant,
    summary: CacheRecoverySummary,
    seen: HashSet<[u8; CHECKSUM_BYTES]>,
}

impl std::fmt::Debug for CacheSnapshotReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CacheSnapshotReader")
            .field("generated_at_utc_millis", &self.generated_at_utc_millis)
            .field("snapshot_bytes", &self.snapshot_bytes)
            .field("remaining", &self.remaining)
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

impl CacheSnapshotReader {
    pub fn next_batch(
        &mut self,
        batch_size: usize,
        deadline: Deadline,
    ) -> Result<Vec<(CacheKey, CacheRecord)>, CacheSnapshotError> {
        if batch_size == 0 {
            return Err(CacheSnapshotError::ResourceExhausted);
        }
        let mut batch = Vec::with_capacity(batch_size.min(self.remaining as usize));
        while self.remaining > 0 && batch.len() < batch_size {
            ensure_deadline(deadline)?;
            let length = read_u32(&mut self.file)?;
            self.remaining -= 1;
            if length > MAX_RECORD_BYTES {
                return Err(CacheSnapshotError::Corrupt);
            }
            let mut payload = vec![0_u8; length as usize];
            self.file.read_exact(&mut payload)?;
            match decode_record(&payload, self.now) {
                Ok((key, record)) => {
                    let digest: [u8; CHECKSUM_BYTES] =
                        Sha256::digest(encode_storage_key(&key).map_err(map_codec_error)?).into();
                    if !self.seen.insert(digest) {
                        self.summary.corrupt = self.summary.corrupt.saturating_add(1);
                        continue;
                    }
                    if is_visible(&record, self.now) {
                        self.summary.loaded = self.summary.loaded.saturating_add(1);
                        batch.push((key, record));
                    } else {
                        self.summary.expired = self.summary.expired.saturating_add(1);
                    }
                }
                Err(CodecError::Incompatible) => {
                    self.summary.incompatible = self.summary.incompatible.saturating_add(1);
                }
                Err(_) => {
                    self.summary.corrupt = self.summary.corrupt.saturating_add(1);
                }
            }
        }
        Ok(batch)
    }

    pub const fn is_complete(&self) -> bool {
        self.remaining == 0
    }

    pub const fn generated_at_utc_millis(&self) -> u64 {
        self.generated_at_utc_millis
    }

    pub const fn snapshot_bytes(&self) -> u64 {
        self.snapshot_bytes
    }

    pub const fn summary(&self) -> CacheRecoverySummary {
        self.summary
    }
}

/// 打开并验证快照。文件不存在表示正常冷启。
pub fn open_cache_snapshot(
    path: &Path,
    max_file_bytes: u64,
    deadline: Deadline,
) -> Result<Option<CacheSnapshotReader>, CacheSnapshotError> {
    ensure_deadline(deadline)?;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() < HEADER_BYTES {
        return Err(CacheSnapshotError::Corrupt);
    }
    if metadata.len() > max_file_bytes {
        return Err(CacheSnapshotError::ResourceExhausted);
    }

    let mut header_bytes = [0_u8; HEADER_BYTES as usize];
    file.read_exact(&mut header_bytes)?;
    let header = decode_header(&header_bytes)?;
    if header.record_count > MAX_RECORDS
        || HEADER_BYTES.checked_add(header.body_bytes) != Some(metadata.len())
    {
        return Err(CacheSnapshotError::Corrupt);
    }

    let mut hasher = Sha256::new();
    let mut remaining = header.body_bytes;
    let mut buffer = [0_u8; VERIFY_BUFFER_BYTES];
    while remaining > 0 {
        ensure_deadline(deadline)?;
        let length = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..length])?;
        hasher.update(&buffer[..length]);
        remaining -= length as u64;
    }
    update_metadata_checksum(&mut hasher, header);
    let actual: [u8; CHECKSUM_BYTES] = hasher.finalize().into();
    if actual != header.checksum {
        return Err(CacheSnapshotError::Corrupt);
    }
    file.seek(SeekFrom::Start(HEADER_BYTES))?;
    Ok(Some(CacheSnapshotReader {
        file,
        generated_at_utc_millis: header.generated_at_utc_millis,
        snapshot_bytes: metadata.len(),
        remaining: header.record_count,
        now: Instant::now(),
        summary: CacheRecoverySummary::default(),
        seen: HashSet::with_capacity(header.record_count as usize),
    }))
}

#[derive(Clone, Copy)]
struct Header {
    generated_at_utc_millis: u64,
    record_count: u32,
    body_bytes: u64,
    checksum: [u8; CHECKSUM_BYTES],
}

fn encode_header(header: Header) -> [u8; HEADER_BYTES as usize] {
    let mut output = [0_u8; HEADER_BYTES as usize];
    output[0..4].copy_from_slice(MAGIC);
    output[4..6].copy_from_slice(&FORMAT_VERSION.to_be_bytes());
    output[6..8].copy_from_slice(&(HEADER_BYTES as u16).to_be_bytes());
    output[8..16].copy_from_slice(&header.generated_at_utc_millis.to_be_bytes());
    output[16..20].copy_from_slice(&header.record_count.to_be_bytes());
    output[20..28].copy_from_slice(&header.body_bytes.to_be_bytes());
    output[28..60].copy_from_slice(&header.checksum);
    output
}

fn decode_header(bytes: &[u8; HEADER_BYTES as usize]) -> Result<Header, CacheSnapshotError> {
    if &bytes[0..4] != MAGIC {
        return Err(CacheSnapshotError::Corrupt);
    }
    if u16::from_be_bytes(bytes[4..6].try_into().unwrap()) != FORMAT_VERSION {
        return Err(CacheSnapshotError::Incompatible);
    }
    if u16::from_be_bytes(bytes[6..8].try_into().unwrap()) != HEADER_BYTES as u16 {
        return Err(CacheSnapshotError::Corrupt);
    }
    Ok(Header {
        generated_at_utc_millis: u64::from_be_bytes(bytes[8..16].try_into().unwrap()),
        record_count: u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        body_bytes: u64::from_be_bytes(bytes[20..28].try_into().unwrap()),
        checksum: bytes[28..60].try_into().unwrap(),
    })
}

fn update_metadata_checksum(hasher: &mut Sha256, header: Header) {
    hasher.update(header.generated_at_utc_millis.to_be_bytes());
    hasher.update(header.record_count.to_be_bytes());
    hasher.update(header.body_bytes.to_be_bytes());
}

fn read_u32(reader: &mut File) -> Result<u32, CacheSnapshotError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn is_visible(record: &CacheRecord, now: Instant) -> bool {
    now < record.entry.expires_at || record.entry.stale_until.is_some_and(|until| now < until)
}

fn map_codec_error(error: CodecError) -> CacheSnapshotError {
    match error {
        CodecError::Corrupt => CacheSnapshotError::Corrupt,
        CodecError::Incompatible => CacheSnapshotError::Incompatible,
        CodecError::ResourceExhausted => CacheSnapshotError::ResourceExhausted,
    }
}

fn ensure_deadline(deadline: Deadline) -> Result<(), CacheSnapshotError> {
    if deadline.is_expired(Instant::now()) {
        Err(CacheSnapshotError::Timeout)
    } else {
        Ok(())
    }
}

fn system_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

fn temporary_path(path: &Path) -> PathBuf {
    let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("cache.snapshot");
    path.with_file_name(format!(".{name}.tmp-{}-{id}", std::process::id()))
}

fn reject_non_regular_target(path: &Path) -> Result<(), CacheSnapshotError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(CacheSnapshotError::Unavailable)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use crate::cache::{CACHE_KEY_FORMAT_VERSION, canonical_checksum};
    use crate::dns::{CanonicalQuery, CanonicalResponse, DnsMessageId, RuntimeRevision};
    use crate::ports::cache::{
        CACHE_ENTRY_FORMAT_VERSION, CacheCondition, CacheEntry, CacheKey, CacheNamespace,
        CacheQuality, CacheResponseClass, CacheStore, CacheUpstreamProvenance,
    };

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn path() -> PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns")
            .join("p2-cache-tests");
        fs::create_dir_all(&directory).unwrap();
        directory.join(format!("snapshot-{}-{id}.bin", std::process::id()))
    }

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn key(value: &str) -> CacheKey {
        CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(value.as_bytes()),
            format_version: CACHE_KEY_FORMAT_VERSION,
        }
    }

    fn entry(expires_at: Instant) -> Arc<CacheEntry> {
        let mut query = Message::new(0, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(
            Name::from_str("snapshot.example.").unwrap(),
            RecordType::A,
        ));
        let canonical_query = CanonicalQuery::from_message(query.clone()).unwrap();
        let mut response = Message::response(0, OpCode::Query);
        response.add_query(query.queries[0].clone());
        let response =
            CanonicalResponse::from_message(response, &canonical_query, DnsMessageId::new(0))
                .unwrap();
        let checksum = canonical_checksum(&response).unwrap();
        Arc::new(CacheEntry {
            response: Arc::new(response),
            upstream: CacheUpstreamProvenance::direct_from_validated_config_id("snapshot").unwrap(),
            inserted_at: Instant::now(),
            expires_at,
            stale_until: None,
            response_class: CacheResponseClass::NoData,
            producer_revision: RuntimeRevision(7),
            quality: CacheQuality::Negative,
            checksum,
            format_version: CACHE_ENTRY_FORMAT_VERSION,
        })
    }

    async fn insert(store: &MokaCacheStore, value: &str, expires_at: Instant) {
        store
            .compare_and_swap(
                key(value),
                CacheCondition::Absent,
                entry(expires_at),
                deadline(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn snapshot_round_trip_uses_bounded_batches_and_preserves_ttl() {
        let path = path();
        let store = MokaCacheStore::with_max_weight(1024 * 1024).unwrap();
        insert(&store, "first", Instant::now() + Duration::from_millis(250)).await;
        insert(&store, "second", Instant::now() + Duration::from_secs(10)).await;

        let written = write_cache_snapshot(&store, &path, 1, deadline()).unwrap();
        assert_eq!(written.records, 2);
        std::thread::sleep(Duration::from_millis(300));

        let mut reader = open_cache_snapshot(&path, 1024 * 1024, deadline())
            .unwrap()
            .unwrap();
        let mut records = Vec::new();
        while !reader.is_complete() {
            records.extend(reader.next_batch(1, deadline()).unwrap());
        }
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, key("second"));
        assert!(reader.is_complete());
        assert_eq!(reader.summary().loaded, 1);
        assert_eq!(reader.summary().expired, 1);
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn corrupt_snapshot_is_rejected_without_partial_recovery() {
        let path = path();
        let store = MokaCacheStore::new();
        insert(&store, "corrupt", Instant::now() + Duration::from_secs(10)).await;
        write_cache_snapshot(&store, &path, 8, deadline()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path, bytes).unwrap();

        assert!(matches!(
            open_cache_snapshot(&path, 1024 * 1024, deadline()),
            Err(CacheSnapshotError::Corrupt)
        ));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn header_count_corruption_is_covered_by_the_integrity_digest() {
        let path = path();
        let store = MokaCacheStore::new();
        insert(&store, "header", Instant::now() + Duration::from_secs(10)).await;
        write_cache_snapshot(&store, &path, 8, deadline()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[19] = bytes[19].wrapping_add(1);
        fs::write(&path, bytes).unwrap();

        assert!(matches!(
            open_cache_snapshot(&path, 1024 * 1024, deadline()),
            Err(CacheSnapshotError::Corrupt)
        ));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn unknown_snapshot_version_is_rejected_as_incompatible() {
        let path = path();
        let store = MokaCacheStore::new();
        insert(&store, "version", Instant::now() + Duration::from_secs(10)).await;
        write_cache_snapshot(&store, &path, 8, deadline()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[5] = bytes[5].wrapping_add(1);
        fs::write(&path, bytes).unwrap();

        assert!(matches!(
            open_cache_snapshot(&path, 1024 * 1024, deadline()),
            Err(CacheSnapshotError::Incompatible)
        ));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn failed_write_keeps_the_previous_complete_snapshot() {
        let path = path();
        let first = MokaCacheStore::new();
        insert(&first, "previous", Instant::now() + Duration::from_secs(10)).await;
        write_cache_snapshot(&first, &path, 8, deadline()).unwrap();

        let second = MokaCacheStore::new();
        insert(
            &second,
            "replacement",
            Instant::now() + Duration::from_secs(10),
        )
        .await;
        assert!(matches!(
            write_cache_snapshot(&second, &path, 0, deadline()),
            Err(CacheSnapshotError::ResourceExhausted)
        ));

        let mut reader = open_cache_snapshot(&path, 1024 * 1024, deadline())
            .unwrap()
            .unwrap();
        let records = reader.next_batch(8, deadline()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, key("previous"));
        drop(reader);

        write_cache_snapshot(&second, &path, 8, deadline()).unwrap();
        let mut reader = open_cache_snapshot(&path, 1024 * 1024, deadline())
            .unwrap()
            .unwrap();
        let records = reader.next_batch(8, deadline()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, key("replacement"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn missing_and_oversized_snapshots_fail_closed() {
        let path = path();
        assert!(
            open_cache_snapshot(&path, 1024, deadline())
                .unwrap()
                .is_none()
        );
        fs::write(&path, [0_u8; HEADER_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            open_cache_snapshot(&path, HEADER_BYTES, deadline()),
            Err(CacheSnapshotError::ResourceExhausted)
        ));
        let _ = fs::remove_file(path);
    }
}
