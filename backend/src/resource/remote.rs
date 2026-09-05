//! 远程 rule-set 的受限拉取、校验和本地原子持久化。
//!
//! 这一层编排 `ResourceFetcher` port 与有效本地快照的条件复用，不实现 HTTP client。
//! 内容文件和 manifest 分别通过临时文件加 `rename` 原子发布；两者
//! 不构成跨文件事务，失败会显式返回错误。

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::config::migrate::deterministic_hash;
use crate::config::model::RuleSetFormat;
use crate::config::resolve::{ConfigId, ResolvedRuleSet};
use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::PortError;
use crate::ports::effects::{
    ProxyProfileId, ResourceContent, ResourceFetchRequest, ResourceFetchResult, ResourceFetcher,
    ResourceLocation, ResourceValidators,
};

use super::{ResourceSnapshot, ResourceSourceKind, ResourceStaleStatus, RuleIndex, RuleLimits};

const MANIFEST_VERSION: u32 = 2;
const PARSER_VERSION: &str = "rule-index-v2";
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
    source_identity: Option<String>,
    validator_scope: Option<String>,
    validators: ResourceValidators,
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
    #[error("remote resource returned 304 without matching validated content")]
    UnexpectedNotModified,
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
    #[serde(default)]
    source_identity: Option<String>,
    #[serde(default)]
    validator_scope: Option<String>,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    last_modified: Option<String>,
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
    // 本地不存在、旧格式或损坏都转为无条件获取；取消/到期则不能被恢复分支吞掉。
    let cached = match restore_remote_rule_set(resource, options.clone()) {
        Ok(loaded)
            if loaded.manifest.validator_scope.as_deref() == fetcher.validator_scope()
                && fetcher.validator_scope().is_some()
                && loaded.manifest.source_identity.as_deref()
                    == Some(&source_identity(resource)) =>
        {
            Some(loaded)
        }
        Err(
            error @ (RemoteResourceError::Cancelled { .. } | RemoteResourceError::DeadlineExceeded),
        ) => return Err(error),
        _ => None,
    };
    let mut request = ResourceFetchRequest {
        location,
        proxy_profile: proxy
            .as_ref()
            .map(|profile| ProxyProfileId(Arc::from(profile.as_str()))),
        max_bytes: options.max_bytes,
        deadline: options.deadline,
        cancellation: options.cancellation.clone(),
        validators: cached
            .as_ref()
            .map(|loaded| loaded.manifest.validators.clone())
            .unwrap_or_default(),
    };
    let mut result = fetcher
        .fetch(request.clone())
        .await
        .map_err(RemoteResourceError::Fetch)?;
    check_boundary(&options)?;
    if let ResourceFetchResult::NotModified(validators) = result {
        validators.validate().map_err(RemoteResourceError::Fetch)?;
        // fetch 期间本地 pair 可能被删除、损坏或被另一代替换，必须再次核验后才能发布。
        if let Some(cached) = cached
            && !request.validators.is_empty()
            && let Ok(mut verified) = restore_remote_rule_set(resource, options.clone())
            && verified.manifest.content_hash == cached.manifest.content_hash
            && verified.manifest.validator_scope == cached.manifest.validator_scope
            && verified.manifest.validators == cached.manifest.validators
        {
            verified.manifest.validators.etag =
                validators.etag.or(verified.manifest.validators.etag);
            verified.manifest.validators.last_modified = validators
                .last_modified
                .or(verified.manifest.validators.last_modified);
            check_boundary(&options)?;
            persist_manifest(&options.manifest_path, &verified.manifest)?;
            check_boundary(&options)?;
            verified.snapshot = ResourceSnapshot::new(
                id.clone(),
                0,
                0,
                verified.manifest.content_hash.clone(),
                verified.snapshot.source_fingerprint().to_owned(),
                PARSER_VERSION,
                SystemTime::now(),
                ResourceSourceKind::Remote,
                false,
                ResourceStaleStatus::Fresh,
                verified.index().clone(),
            );
            return Ok(verified);
        }
        check_boundary(&options)?;
        request.validators = ResourceValidators::default();
        result = fetcher
            .fetch(request)
            .await
            .map_err(RemoteResourceError::Fetch)?;
    }
    check_boundary(&options)?;
    let ResourceFetchResult::Modified(result) = result else {
        return Err(RemoteResourceError::UnexpectedNotModified);
    };
    validate_result(&result, options.max_bytes)?;

    let index = match format {
        RuleSetFormat::Dat => {
            RuleIndex::parse_bytes_with_limits(&result.body, format, options.rule_limits)
        }
        RuleSetFormat::Json | RuleSetFormat::Clash => {
            let text =
                std::str::from_utf8(&result.body).map_err(|_| RemoteResourceError::InvalidUtf8)?;
            RuleIndex::parse_with_limits(text, format, options.rule_limits)
        }
    }
    .map_err(RemoteResourceError::Parse)?;
    check_boundary(&options)?;
    let manifest = RemoteResourceManifest {
        manifest_version: MANIFEST_VERSION,
        resource_id: id.clone(),
        format,
        byte_len: result.body.len(),
        checksum: result.checksum,
        content_hash: Arc::from(deterministic_hash(&result.body)),
        modified_at_unix_nanos: result.modified_at.and_then(unix_nanos),
        parser_version: PARSER_VERSION,
        source_identity: Some(source_identity(resource)),
        validator_scope: fetcher.validator_scope().map(str::to_owned),
        validators: result.validators.clone(),
    };
    persist_content(&options.content_path, &result)?;
    persist_manifest(&options.manifest_path, &manifest)?;
    check_boundary(&options)?;

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

    if !matches!(persisted.manifest_version, 1 | MANIFEST_VERSION)
        || persisted.resource_id != id.as_str()
        || persisted.byte_len != content.len()
        || persisted.parser_version != PARSER_VERSION
        || persisted.format != format_name(format)
        || persisted.content_hash != deterministic_hash(&content)
        || (persisted.manifest_version == MANIFEST_VERSION && persisted.source_identity.is_none())
        || persisted
            .source_identity
            .as_ref()
            .is_some_and(|identity| identity != &source_identity(resource))
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

    let index = match format {
        RuleSetFormat::Dat => {
            RuleIndex::parse_bytes_with_limits(&content, format, options.rule_limits)
        }
        RuleSetFormat::Json | RuleSetFormat::Clash => {
            let text =
                std::str::from_utf8(&content).map_err(|_| RemoteResourceError::InvalidUtf8)?;
            RuleIndex::parse_with_limits(text, format, options.rule_limits)
        }
    }
    .map_err(RemoteResourceError::Parse)?;
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
        source_identity: persisted.source_identity,
        validator_scope: persisted.validator_scope,
        validators: ResourceValidators {
            etag: persisted.etag.map(Arc::from),
            last_modified: persisted.last_modified.map(Arc::from),
        },
    };
    manifest
        .validators
        .validate()
        .map_err(|_| RemoteResourceError::ManifestInvalid)?;
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

/// 只保存定长身份摘要，不将 URL 或代理标识写入 manifest；adapter scope 隔离配置代际。
fn source_identity(resource: &ResolvedRuleSet) -> String {
    let mut hasher = Sha256::new();
    if let ResolvedRuleSet::Remote {
        id,
        url,
        proxy,
        format,
        ..
    } = resource
    {
        for part in [
            id.as_str(),
            url.as_str(),
            proxy.as_ref().map_or("", ConfigId::as_str),
            format_name(*format),
            PARSER_VERSION,
        ] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
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

fn validate_result(result: &ResourceContent, max_bytes: usize) -> Result<(), RemoteResourceError> {
    if result.body.len() > max_bytes {
        return Err(RemoteResourceError::TooLarge {
            actual: result.body.len(),
            max: max_bytes,
        });
    }
    result
        .validators
        .validate()
        .map_err(RemoteResourceError::Fetch)
}

fn persist_content(path: &Path, result: &ResourceContent) -> Result<(), RemoteResourceError> {
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
        source_identity: manifest.source_identity.clone(),
        validator_scope: manifest.validator_scope.clone(),
        etag: manifest.validators.etag.as_deref().map(str::to_owned),
        last_modified: manifest
            .validators
            .last_modified
            .as_deref()
            .map(str::to_owned),
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
                Ok(ResourceFetchResult::Modified(ResourceContent {
                    body,
                    checksum: 42,
                    modified_at: Some(SystemTime::UNIX_EPOCH),
                    validators: ResourceValidators::default(),
                }))
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

    struct ConditionalFetcher {
        scope: &'static str,
        replies: std::sync::Mutex<std::collections::VecDeque<ResourceFetchResult>>,
        requests: std::sync::Mutex<Vec<ResourceFetchRequest>>,
        corrupt_on_304: Option<PathBuf>,
        delay: Duration,
    }

    impl ConditionalFetcher {
        fn new(scope: &'static str, replies: Vec<ResourceFetchResult>) -> Self {
            Self {
                scope,
                replies: std::sync::Mutex::new(replies.into()),
                requests: Default::default(),
                corrupt_on_304: None,
                delay: Duration::ZERO,
            }
        }
    }

    impl ResourceFetcher for ConditionalFetcher {
        fn validator_scope(&self) -> Option<&str> {
            Some(self.scope)
        }

        fn fetch(
            &self,
            request: ResourceFetchRequest,
        ) -> PortFuture<'_, Result<ResourceFetchResult, PortError>> {
            self.requests.lock().unwrap().push(request);
            let response = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected fetch");
            Box::pin(async move {
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                if matches!(response, ResourceFetchResult::NotModified(_))
                    && let Some(path) = &self.corrupt_on_304
                {
                    fs::write(path, b"corrupt").unwrap();
                }
                Ok(response)
            })
        }
    }

    fn conditional_root() -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/tests/resources")
            .join(format!(
                "{}-{}",
                std::process::id(),
                TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn modified(body: &'static [u8]) -> ResourceFetchResult {
        ResourceFetchResult::Modified(ResourceContent {
            body: Arc::from(body),
            checksum: 42,
            modified_at: None,
            validators: ResourceValidators {
                etag: Some(Arc::from("\"private-validator\"")),
                last_modified: None,
            },
        })
    }

    fn unchanged() -> ResourceFetchResult {
        ResourceFetchResult::NotModified(ResourceValidators::default())
    }

    #[tokio::test]
    async fn not_modified_reuses_verified_content_without_changing_content_identity() {
        let root = conditional_root();
        let resource = remote(RuleSetFormat::Clash, "https://rules.example.test/private");
        let fetcher = ConditionalFetcher::new(
            "first",
            vec![modified(b"DOMAIN-SUFFIX,example.test\n"), unchanged()],
        );
        let first = fetch_remote_rule_set(&fetcher, &resource, options(&root))
            .await
            .unwrap();
        let before = fs::read(root.join("rules.txt")).unwrap();
        let second = fetch_remote_rule_set(&fetcher, &resource, options(&root))
            .await
            .unwrap();
        assert_eq!(
            first.snapshot().content_hash(),
            second.snapshot().content_hash()
        );
        assert_eq!(before, fs::read(root.join("rules.txt")).unwrap());
        assert!(!second.snapshot().used_fallback());
        assert!(second.snapshot().fetched_at() >= first.snapshot().fetched_at());
        let requests = fetcher.requests.lock().unwrap();
        assert!(requests[0].validators.is_empty());
        assert_eq!(
            requests[1].validators.etag.as_deref(),
            Some("\"private-validator\"")
        );
        assert!(!format!("{second:?}").contains("private-validator"));
    }

    #[tokio::test]
    async fn missing_or_changed_local_pair_forces_one_unconditional_retry() {
        for corrupt_during_fetch in [false, true] {
            let root = conditional_root();
            let resource = remote(RuleSetFormat::Clash, "https://rules.example.test/rules");
            let initial =
                ConditionalFetcher::new("same", vec![modified(b"DOMAIN-SUFFIX,old.test\n")]);
            fetch_remote_rule_set(&initial, &resource, options(&root))
                .await
                .unwrap();
            let mut fetcher = ConditionalFetcher::new(
                "same",
                vec![unchanged(), modified(b"DOMAIN-SUFFIX,new.test\n")],
            );
            if corrupt_during_fetch {
                fetcher.corrupt_on_304 = Some(root.join("rules.txt"));
            } else {
                fs::remove_file(root.join("rules.txt")).unwrap();
            }
            let loaded = fetch_remote_rule_set(&fetcher, &resource, options(&root))
                .await
                .unwrap();
            assert_eq!(loaded.index().suffix_count(), 1);
            let requests = fetcher.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].validators.is_empty(), !corrupt_during_fetch);
            assert!(requests[1].validators.is_empty());
            assert_eq!(requests[0].deadline.at(), requests[1].deadline.at());
            assert_eq!(
                fs::read(root.join("rules.txt")).unwrap(),
                b"DOMAIN-SUFFIX,new.test\n"
            );
        }
    }

    #[tokio::test]
    async fn url_or_adapter_generation_change_does_not_reuse_old_validators() {
        for change_url in [false, true] {
            let root = conditional_root();
            let original = remote(RuleSetFormat::Clash, "https://rules.example.test/original");
            let first = ConditionalFetcher::new(
                "generation-a",
                vec![modified(b"DOMAIN-SUFFIX,old.test\n")],
            );
            fetch_remote_rule_set(&first, &original, options(&root))
                .await
                .unwrap();
            let next = if change_url {
                remote(RuleSetFormat::Clash, "https://rules.example.test/new")
            } else {
                original
            };
            let fetcher = ConditionalFetcher::new(
                if change_url {
                    "generation-a"
                } else {
                    "generation-b"
                },
                vec![modified(b"DOMAIN-SUFFIX,new.test\n")],
            );
            fetch_remote_rule_set(&fetcher, &next, options(&root))
                .await
                .unwrap();
            assert!(fetcher.requests.lock().unwrap()[0].validators.is_empty());
        }
    }

    #[tokio::test]
    async fn invalid_new_content_or_repeated_304_cannot_replace_a_valid_snapshot() {
        let root = conditional_root();
        let resource = remote(RuleSetFormat::Json, "https://rules.example.test/rules");
        let first = ConditionalFetcher::new(
            "same",
            vec![modified(
                b"{\"version\":2,\"rules\":[{\"domain_suffix\":\"example.test\"}]}",
            )],
        );
        fetch_remote_rule_set(&first, &resource, options(&root))
            .await
            .unwrap();
        let before = fs::read(root.join("rules.txt")).unwrap();
        let manifest = fs::read(root.join("rules.manifest")).unwrap();
        let invalid = ConditionalFetcher::new("same", vec![modified(b"invalid json")]);
        assert!(
            fetch_remote_rule_set(&invalid, &resource, options(&root))
                .await
                .is_err()
        );
        assert_eq!(before, fs::read(root.join("rules.txt")).unwrap());
        assert_eq!(manifest, fs::read(root.join("rules.manifest")).unwrap());
        let repeated = ConditionalFetcher::new("other-generation", vec![unchanged(), unchanged()]);
        assert!(matches!(
            fetch_remote_rule_set(&repeated, &resource, options(&root)).await,
            Err(RemoteResourceError::UnexpectedNotModified),
        ));
        assert_eq!(repeated.requests.lock().unwrap().len(), 2);
        assert_eq!(manifest, fs::read(root.join("rules.manifest")).unwrap());
    }

    #[tokio::test]
    async fn conditional_retry_never_receives_a_fresh_deadline() {
        let root = conditional_root();
        let mut fetcher = ConditionalFetcher::new("same", vec![unchanged()]);
        fetcher.delay = Duration::from_millis(20);
        let mut options = options(&root);
        options.deadline = Deadline::new(Instant::now() + Duration::from_millis(5));
        assert!(matches!(
            fetch_remote_rule_set(
                &fetcher,
                &remote(RuleSetFormat::Clash, "https://rules.example.test/rules"),
                options
            )
            .await,
            Err(RemoteResourceError::DeadlineExceeded),
        ));
        assert!(fetcher.requests.lock().unwrap().len() <= 1);
    }

    #[tokio::test]
    async fn cancellation_after_304_prevents_unconditional_retry() {
        let root = conditional_root();
        let mut fetcher = ConditionalFetcher::new("same", vec![unchanged()]);
        fetcher.delay = Duration::from_millis(20);
        let options = options(&root);
        let cancellation = options.cancellation.clone();
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            cancellation.cancel(CancelReason::Shutdown);
        };
        let resource = remote(RuleSetFormat::Clash, "https://rules.example.test/rules");
        let (result, ()) =
            tokio::join!(fetch_remote_rule_set(&fetcher, &resource, options), cancel,);
        assert!(matches!(
            result,
            Err(RemoteResourceError::Cancelled {
                reason: CancelReason::Shutdown
            })
        ));
        assert_eq!(fetcher.requests.lock().unwrap().len(), 1);
        assert!(!root.join("rules.manifest").exists());
    }

    #[tokio::test]
    async fn legacy_manifest_restores_but_cannot_supply_conditional_validators() {
        let root = conditional_root();
        let resource = remote(RuleSetFormat::Clash, "https://rules.example.test/rules");
        let first =
            ConditionalFetcher::new("same", vec![modified(b"DOMAIN-SUFFIX,example.test\n")]);
        fetch_remote_rule_set(&first, &resource, options(&root))
            .await
            .unwrap();
        let mut manifest: PersistedManifest =
            yaml_serde::from_slice(&fs::read(root.join("rules.manifest")).unwrap()).unwrap();
        manifest.manifest_version = 1;
        manifest.source_identity = None;
        manifest.validator_scope = None;
        manifest.etag = None;
        manifest.last_modified = None;
        fs::write(
            root.join("rules.manifest"),
            yaml_serde::to_string(&manifest).unwrap(),
        )
        .unwrap();
        assert!(
            restore_remote_rule_set(&resource, options(&root))
                .unwrap()
                .snapshot()
                .used_fallback()
        );
        let next = ConditionalFetcher::new("same", vec![modified(b"DOMAIN-SUFFIX,example.test\n")]);
        fetch_remote_rule_set(&next, &resource, options(&root))
            .await
            .unwrap();
        assert!(next.requests.lock().unwrap()[0].validators.is_empty());
    }

    fn dat_fixture() -> Arc<[u8]> {
        let mut bytes = vec![
            0x0a, 0x16, 0x0a, 0x02, b'C', b'N', 0x12, 0x10, 0x08, 0x03, 0x12, 0x0c,
        ];
        bytes.extend_from_slice(b"example.test");
        Arc::from(bytes)
    }

    #[tokio::test]
    async fn fetches_valid_rule_set_and_persists_safe_metadata_atomically() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-remote-{}-{}",
            std::process::id(),
            TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let body: Arc<[u8]> = Arc::from(
            &b"{\"version\":2,\"rules\":[{\"domain_suffix\":\"example.test\",\"invert\":true}]}"[..],
        );
        let fetcher = FakeFetcher {
            body: Arc::clone(&body),
            calls: Arc::clone(&calls),
        };
        let loaded = fetch_remote_rule_set(
            &fetcher,
            &remote(RuleSetFormat::Json, "https://rules.example.test/private"),
            options(&root),
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(loaded.index().suffix_count(), 1);
        assert_eq!(loaded.manifest().byte_len(), body.len());
        assert_eq!(fs::read(root.join("rules.txt")).unwrap().len(), body.len());
        assert_eq!(loaded.manifest().parser_version(), "rule-index-v2");
        let manifest = fs::read_to_string(root.join("rules.manifest")).unwrap();
        assert!(manifest.contains("manifest_version: 2"));
        assert!(manifest.contains("parser_version: rule-index-v2"));
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
    async fn rejects_oversize_and_invalid_utf8_then_persists_dat() {
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
            body: dat_fixture(),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let resource = remote(RuleSetFormat::Dat, "https://rules.example.test/rules");
        let loaded = fetch_remote_rule_set(&valid, &resource, options(&root))
            .await
            .unwrap();
        assert!(loaded.index().selector("cn").is_some());
        let restored = restore_remote_rule_set(&resource, options(&root)).unwrap();
        assert!(restored.index().selector("cn").is_some());
        assert!(root.join("rules.txt").exists());
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
