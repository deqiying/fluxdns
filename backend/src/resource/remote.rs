//! 远程 rule-set 的受限拉取、校验和本地原子持久化。
//!
//! 这一层只编排已有的 `ResourceFetcher` port，不实现 HTTP client、304 协商或
//! fallback。内容文件和 manifest 分别通过临时文件加 `rename` 原子发布；两者
//! 不构成跨文件事务，失败会显式返回错误。

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::config::migrate::deterministic_hash;
use crate::config::model::RuleSetFormat;
use crate::config::resolve::{ConfigId, ResolvedRuleSet};
use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::PortError;
use crate::ports::effects::{
    ProxyProfileId, ResourceFetchRequest, ResourceFetchResult, ResourceFetcher, ResourceLocation,
};

use super::{ResourceSnapshot, ResourceSourceKind, ResourceStaleStatus, RuleIndex, RuleLimits};

const MANIFEST_VERSION: u32 = 1;
const PARSER_VERSION: &str = "rule-index-v1";
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 远程资源本次拉取的边界和持久化目标。
#[derive(Clone, Debug)]
pub struct RemoteResourceOptions {
    pub max_bytes: usize,
    pub rule_limits: RuleLimits,
    pub content_path: PathBuf,
    pub manifest_path: PathBuf,
    pub deadline: Deadline,
    pub cancellation: Cancellation,
}

impl RemoteResourceOptions {
    pub fn new(
        max_bytes: usize,
        content_path: impl Into<PathBuf>,
        manifest_path: impl Into<PathBuf>,
        deadline: Deadline,
        cancellation: Cancellation,
    ) -> Self {
        Self {
            max_bytes,
            rule_limits: RuleLimits::default(),
            content_path: content_path.into(),
            manifest_path: manifest_path.into(),
            deadline,
            cancellation,
        }
    }
}

/// 已完成校验并完成本地原子发布的远程 rule-set。
#[derive(Clone)]
pub struct LoadedRemoteRuleSet {
    id: ConfigId,
    manifest: RemoteResourceManifest,
    snapshot: ResourceSnapshot<RuleIndex>,
}

impl LoadedRemoteRuleSet {
    pub fn id(&self) -> &ConfigId {
        &self.id
    }

    pub fn manifest(&self) -> &RemoteResourceManifest {
        &self.manifest
    }

    pub fn snapshot(&self) -> &ResourceSnapshot<RuleIndex> {
        &self.snapshot
    }

    pub(crate) fn with_snapshot(self, snapshot: ResourceSnapshot<RuleIndex>) -> Self {
        Self { snapshot, ..self }
    }

    pub fn index(&self) -> &RuleIndex {
        self.snapshot.compiled()
    }
}

impl fmt::Debug for LoadedRemoteRuleSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedRemoteRuleSet")
            .field("id", &"[REDACTED]")
            .field("manifest", &self.manifest)
            .field("snapshot", &self.snapshot)
            .finish()
    }
}

/// 本地 manifest 的安全元数据；不保存 URL、proxy、正文或路径。
#[derive(Clone)]
pub struct RemoteResourceManifest {
    manifest_version: u32,
    resource_id: ConfigId,
    format: RuleSetFormat,
    byte_len: usize,
    checksum: u64,
    content_hash: Arc<str>,
    modified_at_unix_nanos: Option<u128>,
    parser_version: &'static str,
}

impl RemoteResourceManifest {
    pub const fn manifest_version(&self) -> u32 {
        self.manifest_version
    }

    pub fn resource_id(&self) -> &ConfigId {
        &self.resource_id
    }

    pub const fn format(&self) -> RuleSetFormat {
        self.format
    }

    pub const fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub const fn checksum(&self) -> u64 {
        self.checksum
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub const fn modified_at_unix_nanos(&self) -> Option<u128> {
        self.modified_at_unix_nanos
    }

    pub const fn parser_version(&self) -> &'static str {
        self.parser_version
    }
}

impl fmt::Debug for RemoteResourceManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteResourceManifest")
            .field("manifest_version", &self.manifest_version)
            .field("resource_id", &"[REDACTED]")
            .field("format", &self.format)
            .field("byte_len", &self.byte_len)
            .field("checksum", &self.checksum)
            .field("content_hash", &"[REDACTED]")
            .field("modified_at_unix_nanos", &self.modified_at_unix_nanos)
            .field("parser_version", &self.parser_version)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum RemoteResourceError {
    #[error("resource source `{kind}` is not remote")]
    UnsupportedSource { kind: &'static str },
    #[error("remote resource source must use http or https")]
    UnsupportedScheme,
    #[error("remote resource source must include a host")]
    MissingHost,
    #[error("remote resource source must not contain credentials or a fragment")]
    UnsafeUrl,
    #[error("remote resource max_bytes must be greater than zero")]
    ZeroMaxBytes,
    #[error("remote resource persistence paths must be distinct files")]
    InvalidPersistencePaths,
    #[error("remote resource request was cancelled")]
    Cancelled { reason: CancelReason },
    #[error("remote resource request exceeded its deadline")]
    DeadlineExceeded,
    #[error("remote resource fetch failed: {0}")]
    Fetch(#[source] PortError),
    #[error("remote resource body exceeds the configured size limit")]
    TooLarge { actual: usize, max: usize },
    #[error("remote resource body is not valid UTF-8")]
    InvalidUtf8,
    #[error("remote rule-set format is unsupported")]
    UnsupportedFormat,
    #[error("remote rule-set could not be parsed: {0}")]
    Parse(#[source] super::RuleParseError),
    #[error("remote resource persistence failed during {operation}")]
    Persistence {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("remote resource manifest serialization failed")]
    ManifestSerialization,
    #[error("remote resource manifest is invalid")]
    ManifestInvalid,
    #[error("remote resource manifest does not match its content")]
    ManifestMismatch,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedManifest {
    manifest_version: u32,
    resource_id: String,
    format: String,
    byte_len: usize,
    checksum: u64,
    content_hash: String,
    modified_at_unix_nanos: Option<u128>,
    parser_version: String,
}

/// 拉取一个 remote rule-set，完成有界解析后分别原子发布 content 与 manifest。
///
/// `ResourceFetcher` 必须自行实现真正的 deadline/cancellation 协作；本函数在
/// fetch 前后再次检查边界，避免已取消或已过期的结果进入本地持久化。
pub async fn fetch_remote_rule_set(
    fetcher: &dyn ResourceFetcher,
    resource: &ResolvedRuleSet,
    options: RemoteResourceOptions,
) -> Result<LoadedRemoteRuleSet, RemoteResourceError> {
    let (id, format, url, proxy) = match resource {
        ResolvedRuleSet::Remote {
            id,
            format,
            url,
            proxy,
            ..
        } => (id, *format, url, proxy),
        ResolvedRuleSet::Const { .. } => {
            return Err(RemoteResourceError::UnsupportedSource { kind: "const" });
        }
        ResolvedRuleSet::File { .. } => {
            return Err(RemoteResourceError::UnsupportedSource { kind: "file" });
        }
    };
    validate_url(url)?;
    validate_options(&options)?;
    check_boundary(&options)?;

    let location = ResourceLocation::new(Arc::<str>::from(url.as_str()))
        .map_err(|error| RemoteResourceError::Fetch(error.with_safe_context("location")))?;
    let request = ResourceFetchRequest {
        location,
        proxy_profile: proxy
            .as_ref()
            .map(|profile| ProxyProfileId(Arc::from(profile.as_str()))),
        max_bytes: options.max_bytes,
        deadline: options.deadline,
        cancellation: options.cancellation.clone(),
    };
    let result = fetcher
        .fetch(request)
        .await
        .map_err(RemoteResourceError::Fetch)?;
    check_boundary(&options)?;
    validate_result(&result, options.max_bytes)?;

    let index = match format {
        RuleSetFormat::Dat => return Err(RemoteResourceError::UnsupportedFormat),
        RuleSetFormat::Json | RuleSetFormat::Clash => {
            let text =
                std::str::from_utf8(&result.body).map_err(|_| RemoteResourceError::InvalidUtf8)?;
            RuleIndex::parse_with_limits(text, format, options.rule_limits)
                .map_err(RemoteResourceError::Parse)?
        }
    };
    let manifest = RemoteResourceManifest {
        manifest_version: MANIFEST_VERSION,
        resource_id: id.clone(),
        format,
        byte_len: result.body.len(),
        checksum: result.checksum,
        content_hash: Arc::from(deterministic_hash(&result.body)),
        modified_at_unix_nanos: result.modified_at.and_then(unix_nanos),
        parser_version: PARSER_VERSION,
    };
    persist_content(&options.content_path, &result)?;
    persist_manifest(&options.manifest_path, &manifest)?;

    let source_fingerprint = format!("remote:{}:{}", result.checksum, result.body.len());
    let snapshot = ResourceSnapshot::new(
        id.clone(),
        0,
        0,
        manifest.content_hash.clone(),
        source_fingerprint,
        PARSER_VERSION,
        result.modified_at.unwrap_or_else(SystemTime::now),
        ResourceSourceKind::Remote,
        false,
        ResourceStaleStatus::Fresh,
        index,
    );
    Ok(LoadedRemoteRuleSet {
        id: id.clone(),
        manifest,
        snapshot,
    })
}

/// 从上一轮成功原子落盘的 content/manifest pair 恢复 remote rule-set。
///
/// 恢复路径只接受当前配置声明的 remote 资源，并同时校验 manifest schema、资源
/// 身份、格式、parser 版本、字节长度和内容 hash。任一校验失败都不会产生可用
/// snapshot，调用方可以继续执行本次 fetch 或将启动失败分类为不可恢复。
pub fn restore_remote_rule_set(
    resource: &ResolvedRuleSet,
    options: RemoteResourceOptions,
) -> Result<LoadedRemoteRuleSet, RemoteResourceError> {
    let (id, format) = match resource {
        ResolvedRuleSet::Remote {
            id, format, url, ..
        } => {
            validate_url(url)?;
            (id, *format)
        }
        ResolvedRuleSet::Const { .. } => {
            return Err(RemoteResourceError::UnsupportedSource { kind: "const" });
        }
        ResolvedRuleSet::File { .. } => {
            return Err(RemoteResourceError::UnsupportedSource { kind: "file" });
        }
    };
    validate_options(&options)?;
    check_boundary(&options)?;

    let manifest_bytes =
        read_persisted_file(&options.manifest_path, MAX_MANIFEST_BYTES, "manifest_read")?;
    let persisted: PersistedManifest = yaml_serde::from_slice(&manifest_bytes)
        .map_err(|_| RemoteResourceError::ManifestInvalid)?;
    let content = read_persisted_file(&options.content_path, options.max_bytes, "content_read")?;
    check_boundary(&options)?;

    if persisted.manifest_version != MANIFEST_VERSION
        || persisted.resource_id != id.as_str()
        || persisted.byte_len != content.len()
        || persisted.parser_version != PARSER_VERSION
        || persisted.format != format_name(format)
        || persisted.content_hash != deterministic_hash(&content)
    {
        return Err(RemoteResourceError::ManifestMismatch);
    }
    let modified_at = persisted
        .modified_at_unix_nanos
        .map(|nanos| {
            u64::try_from(nanos)
                .ok()
                .and_then(|nanos| UNIX_EPOCH.checked_add(Duration::from_nanos(nanos)))
                .ok_or(RemoteResourceError::ManifestInvalid)
        })
        .transpose()?;

    let text = std::str::from_utf8(&content).map_err(|_| RemoteResourceError::InvalidUtf8)?;
    let index = match format {
        RuleSetFormat::Dat => return Err(RemoteResourceError::UnsupportedFormat),
        RuleSetFormat::Json | RuleSetFormat::Clash => {
            RuleIndex::parse_with_limits(text, format, options.rule_limits)
                .map_err(RemoteResourceError::Parse)?
        }
    };
    let content_hash: Arc<str> = Arc::from(persisted.content_hash);
    let manifest = RemoteResourceManifest {
        manifest_version: persisted.manifest_version,
        resource_id: id.clone(),
        format,
        byte_len: persisted.byte_len,
        checksum: persisted.checksum,
        content_hash: Arc::clone(&content_hash),
        modified_at_unix_nanos: persisted.modified_at_unix_nanos,
        parser_version: PARSER_VERSION,
    };
    let snapshot = ResourceSnapshot::new(
        id.clone(),
        0,
        0,
        content_hash,
        format!("remote:{}:{}", manifest.checksum, manifest.byte_len),
        PARSER_VERSION,
        modified_at.unwrap_or_else(SystemTime::now),
        ResourceSourceKind::Remote,
        true,
        ResourceStaleStatus::Fresh,
        index,
    );
    Ok(LoadedRemoteRuleSet {
        id: id.clone(),
        manifest,
        snapshot,
    })
}

fn validate_url(url: &Url) -> Result<(), RemoteResourceError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(RemoteResourceError::UnsupportedScheme);
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(RemoteResourceError::MissingHost);
    }
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return Err(RemoteResourceError::UnsafeUrl);
    }
    Ok(())
}

fn validate_options(options: &RemoteResourceOptions) -> Result<(), RemoteResourceError> {
    if options.max_bytes == 0 || options.rule_limits.max_input_bytes == 0 {
        return Err(RemoteResourceError::ZeroMaxBytes);
    }
    if options.content_path.as_os_str().is_empty()
        || options.manifest_path.as_os_str().is_empty()
        || options.content_path == options.manifest_path
    {
        return Err(RemoteResourceError::InvalidPersistencePaths);
    }
    Ok(())
}

fn check_boundary(options: &RemoteResourceOptions) -> Result<(), RemoteResourceError> {
    if options.cancellation.is_cancelled() {
        return Err(RemoteResourceError::Cancelled {
            reason: options
                .cancellation
                .reason()
                .unwrap_or(CancelReason::Shutdown),
        });
    }
    if options.deadline.is_expired(Instant::now()) {
        return Err(RemoteResourceError::DeadlineExceeded);
    }
    Ok(())
}

fn validate_result(
    result: &ResourceFetchResult,
    max_bytes: usize,
) -> Result<(), RemoteResourceError> {
    if result.body.len() > max_bytes {
        return Err(RemoteResourceError::TooLarge {
            actual: result.body.len(),
            max: max_bytes,
        });
    }
    Ok(())
}

fn persist_content(path: &Path, result: &ResourceFetchResult) -> Result<(), RemoteResourceError> {
    atomic_write(path, &result.body, "content")
}

fn persist_manifest(
    path: &Path,
    manifest: &RemoteResourceManifest,
) -> Result<(), RemoteResourceError> {
    let persisted = PersistedManifest {
        manifest_version: manifest.manifest_version,
        resource_id: manifest.resource_id.as_str().to_owned(),
        format: format_name(manifest.format).to_owned(),
        byte_len: manifest.byte_len,
        checksum: manifest.checksum,
        content_hash: manifest.content_hash.to_string(),
        modified_at_unix_nanos: manifest.modified_at_unix_nanos,
        parser_version: manifest.parser_version.to_owned(),
    };
    let text = yaml_serde::to_string(&persisted)
        .map_err(|_| RemoteResourceError::ManifestSerialization)?;
    atomic_write(path, text.as_bytes(), "manifest")
}

fn read_persisted_file(
    path: &Path,
    max_bytes: usize,
    operation: &'static str,
) -> Result<Vec<u8>, RemoteResourceError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| RemoteResourceError::Persistence { operation, source })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RemoteResourceError::ManifestInvalid);
    }
    let byte_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if byte_len > max_bytes {
        return Err(RemoteResourceError::TooLarge {
            actual: byte_len,
            max: max_bytes,
        });
    }
    let bytes =
        fs::read(path).map_err(|source| RemoteResourceError::Persistence { operation, source })?;
    if bytes.len() != byte_len {
        return Err(RemoteResourceError::ManifestInvalid);
    }
    Ok(bytes)
}

const fn format_name(format: RuleSetFormat) -> &'static str {
    match format {
        RuleSetFormat::Json => "json",
        RuleSetFormat::Clash => "clash",
        RuleSetFormat::Dat => "dat",
    }
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    operation: &'static str,
) -> Result<(), RemoteResourceError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|source| RemoteResourceError::Persistence { operation, source })?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = path.with_extension(format!("tmp-{}-{}", std::process::id(), sequence));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|source| RemoteResourceError::Persistence { operation, source })?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|source| RemoteResourceError::Persistence { operation, source })?;
        fs::rename(&temp_path, path)
            .map_err(|source| RemoteResourceError::Persistence { operation, source })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn unix_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::ports::{PortErrorClass, PortFuture};

    struct FakeFetcher {
        body: Arc<[u8]>,
        calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for FakeFetcher {
        fn fetch(
            &self,
            _request: ResourceFetchRequest,
        ) -> PortFuture<'_, Result<ResourceFetchResult, PortError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let body = Arc::clone(&self.body);
            Box::pin(async move {
                Ok(ResourceFetchResult {
                    body,
                    checksum: 42,
                    modified_at: Some(SystemTime::UNIX_EPOCH),
                })
            })
        }
    }

    fn remote(format: RuleSetFormat, url: &str) -> ResolvedRuleSet {
        ResolvedRuleSet::Remote {
            id: ConfigId::new("remote-rules").unwrap(),
            format,
            url: Url::parse(url).unwrap(),
            proxy: None,
            auto_update: false,
            update_interval: None,
        }
    }

    fn options(root: &Path) -> RemoteResourceOptions {
        RemoteResourceOptions::new(
            1024,
            root.join("rules.txt"),
            root.join("rules.manifest"),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
    }

    #[tokio::test]
    async fn fetches_valid_rule_set_and_persists_safe_metadata_atomically() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-remote-{}-{}",
            std::process::id(),
            TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = FakeFetcher {
            body: Arc::from(&b"DOMAIN-SUFFIX,example.test\n"[..]),
            calls: Arc::clone(&calls),
        };
        let loaded = fetch_remote_rule_set(
            &fetcher,
            &remote(RuleSetFormat::Clash, "https://rules.example.test/private"),
            options(&root),
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(loaded.index().suffix_count(), 1);
        assert_eq!(loaded.manifest().byte_len(), 27);
        assert_eq!(fs::read(root.join("rules.txt")).unwrap().len(), 27);
        let manifest = fs::read_to_string(root.join("rules.manifest")).unwrap();
        assert!(manifest.contains("manifest_version: 1"));
        assert!(manifest.contains("content_hash:"));
        assert!(!manifest.contains("rules.example.test"));
        assert!(!format!("{loaded:?}").contains("example.test"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn restores_a_valid_persisted_pair_as_a_fallback_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-remote-restore-{}-{}",
            std::process::id(),
            TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let fetcher = FakeFetcher {
            body: Arc::from(&b"DOMAIN-SUFFIX,example.test\n"[..]),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let loaded = fetch_remote_rule_set(
            &fetcher,
            &remote(RuleSetFormat::Clash, "https://rules.example.test/private"),
            options(&root),
        )
        .await
        .unwrap();
        let restored = restore_remote_rule_set(
            &remote(RuleSetFormat::Clash, "https://rules.example.test/private"),
            options(&root),
        )
        .unwrap();

        assert_eq!(restored.id(), loaded.id());
        assert_eq!(
            restored.manifest().content_hash(),
            loaded.manifest().content_hash()
        );
        assert!(restored.snapshot().used_fallback());
        assert_eq!(restored.index().suffix_count(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_tampered_or_mismatched_persisted_pairs() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-remote-restore-reject-{}-{}",
            std::process::id(),
            TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let fetcher = FakeFetcher {
            body: Arc::from(&b"DOMAIN-SUFFIX,example.test\n"[..]),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        fetch_remote_rule_set(
            &fetcher,
            &remote(RuleSetFormat::Clash, "https://rules.example.test/private"),
            options(&root),
        )
        .await
        .unwrap();

        fs::write(root.join("rules.txt"), b"DOMAIN-SUFFIX,changed.test\n").unwrap();
        assert!(matches!(
            restore_remote_rule_set(
                &remote(RuleSetFormat::Clash, "https://rules.example.test/private"),
                options(&root),
            ),
            Err(RemoteResourceError::ManifestMismatch)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_invalid_boundaries_before_fetch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = FakeFetcher {
            body: Arc::from(&b"ignored"[..]),
            calls: Arc::clone(&calls),
        };
        let root = std::env::temp_dir().join("fluxdns-remote-boundary");
        let cancelled = options(&root);
        cancelled
            .cancellation
            .cancel(CancelReason::ClientDisconnected);
        assert!(matches!(
            fetch_remote_rule_set(
                &fetcher,
                &remote(RuleSetFormat::Clash, "https://rules.example.test/rules"),
                cancelled,
            )
            .await,
            Err(RemoteResourceError::Cancelled {
                reason: CancelReason::ClientDisconnected
            })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);

        assert!(matches!(
            fetch_remote_rule_set(
                &fetcher,
                &remote(
                    RuleSetFormat::Clash,
                    "https://user:password@rules.example.test/rules"
                ),
                options(&root),
            )
            .await,
            Err(RemoteResourceError::UnsafeUrl)
        ));

        let mut zero_bytes = options(&root);
        zero_bytes.max_bytes = 0;
        assert!(matches!(
            fetch_remote_rule_set(
                &fetcher,
                &remote(RuleSetFormat::Clash, "https://rules.example.test/rules"),
                zero_bytes,
            )
            .await,
            Err(RemoteResourceError::ZeroMaxBytes)
        ));

        let expired = RemoteResourceOptions::new(
            1024,
            root.join("rules.txt"),
            root.join("rules.manifest"),
            Deadline::new(Instant::now() - Duration::from_secs(1)),
            Cancellation::new(),
        );
        assert!(matches!(
            fetch_remote_rule_set(
                &fetcher,
                &remote(RuleSetFormat::Clash, "ftp://rules.example.test/rules"),
                expired,
            )
            .await,
            Err(RemoteResourceError::UnsupportedScheme)
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn rejects_oversize_utf8_and_dat_before_persistence() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-remote-reject-{}",
            TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let fetcher = FakeFetcher {
            body: Arc::from([0xff, 0xfe]),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let mut bounded = options(&root);
        bounded.max_bytes = 1;
        assert!(matches!(
            fetch_remote_rule_set(
                &fetcher,
                &remote(RuleSetFormat::Json, "https://rules.example.test/rules"),
                bounded,
            )
            .await,
            Err(RemoteResourceError::TooLarge { actual: 2, max: 1 })
        ));

        let mut utf8 = options(&root);
        utf8.max_bytes = 2;
        assert!(matches!(
            fetch_remote_rule_set(
                &fetcher,
                &remote(RuleSetFormat::Json, "https://rules.example.test/rules"),
                utf8,
            )
            .await,
            Err(RemoteResourceError::InvalidUtf8)
        ));

        let valid = FakeFetcher {
            body: Arc::from(&b"opaque"[..]),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        assert!(matches!(
            fetch_remote_rule_set(
                &valid,
                &remote(RuleSetFormat::Dat, "https://rules.example.test/rules"),
                options(&root),
            )
            .await,
            Err(RemoteResourceError::UnsupportedFormat)
        ));
        assert!(!root.join("rules.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn debug_and_error_do_not_include_url_or_body() {
        let error = RemoteResourceError::Fetch(PortError::new(
            PortErrorClass::Unavailable,
            "resource_fetch",
        ));
        let debug = format!("{error:?}");
        assert!(!debug.contains("rules.example"));
        assert!(!debug.contains("private-body"));
    }
}
