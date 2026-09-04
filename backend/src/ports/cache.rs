//! 内存缓存与独立持久化缓存的最小契约。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::dns::{CancelReason, Cancellation, CanonicalResponse, Deadline, RuntimeRevision};

use super::{PortError, PortFuture};

/// 当前 cache entry payload 格式版本。
pub const CACHE_ENTRY_FORMAT_VERSION: u16 = 2;

/// 缓存策略的稳定标识。
///
/// 该值只能来自配置编译后的受控标识，不能使用租户名、规则正文或其他自由文本。
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct CacheStrategyId(Arc<str>);

impl CacheStrategyId {
    /// 由已通过 Config 校验的稳定标识构造 namespace 组件。
    pub fn from_validated_config_id(value: &str) -> Result<Self, CacheNamespaceIdError> {
        validate_namespace_id(value, 1, 64).map(|()| Self(Arc::from(value)))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for CacheStrategyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CacheStrategyId(REDACTED)")
    }
}

/// 客户端维度缓存 namespace 使用的不可逆摘要标识。
///
/// 保留为 opaque type，避免调用方把原始 client 地址或身份材料放入 cache key。
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ClientCacheDigest([u8; 32]);

impl ClientCacheDigest {
    /// 由上层带域分隔的摘要函数提供 opaque digest。
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub(crate) const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for ClientCacheDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClientCacheDigest(REDACTED)")
    }
}

/// cache namespace 标识格式错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheNamespaceIdError {
    Empty,
    TooShort,
    TooLong,
    InvalidCharacter,
}

fn validate_namespace_id(
    value: &str,
    minimum_len: usize,
    maximum_len: usize,
) -> Result<(), CacheNamespaceIdError> {
    if value.is_empty() {
        return Err(CacheNamespaceIdError::Empty);
    }
    if value.len() < minimum_len {
        return Err(CacheNamespaceIdError::TooShort);
    }
    if value.len() > maximum_len {
        return Err(CacheNamespaceIdError::TooLong);
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(CacheNamespaceIdError::InvalidCharacter);
    }
    Ok(())
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub enum CacheNamespace {
    Global,
    Strategy(CacheStrategyId),
    ClientStrategy {
        client_digest: ClientCacheDigest,
        strategy: CacheStrategyId,
    },
}

impl fmt::Debug for CacheNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => formatter.write_str("CacheNamespace::Global"),
            Self::Strategy(_) => formatter.write_str("CacheNamespace::Strategy(REDACTED)"),
            Self::ClientStrategy { .. } => {
                formatter.write_str("CacheNamespace::ClientStrategy(REDACTED)")
            }
        }
    }
}

/// 缓存来源使用的稳定 upstream 配置标识。
///
/// 该值只能由已经通过配置边界校验的 ID 构造，不能保存 URL、地址或 SecretRef。
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct CacheUpstreamId(Arc<str>);

impl CacheUpstreamId {
    pub fn from_validated_config_id(value: &str) -> Result<Self, CacheUpstreamIdError> {
        if value.is_empty() || value.len() > 128 {
            return Err(CacheUpstreamIdError);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!'))
        {
            return Err(CacheUpstreamIdError);
        }
        Ok(Self(Arc::from(value)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CacheUpstreamId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CacheUpstreamId(REDACTED)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheUpstreamIdError;

impl fmt::Display for CacheUpstreamIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("cache upstream id must be a validated configuration identifier")
    }
}

impl std::error::Error for CacheUpstreamIdError {}

/// 产生缓存响应的 upstream target 与实际 direct/member。
#[derive(Clone, Eq, PartialEq)]
pub struct CacheUpstreamProvenance {
    target_id: CacheUpstreamId,
    used_id: Option<CacheUpstreamId>,
}

impl CacheUpstreamProvenance {
    pub fn new(target_id: CacheUpstreamId, used_id: Option<CacheUpstreamId>) -> Self {
        Self { target_id, used_id }
    }

    pub fn direct_from_validated_config_id(value: &str) -> Result<Self, CacheUpstreamIdError> {
        let id = CacheUpstreamId::from_validated_config_id(value)?;
        Ok(Self::new(id.clone(), Some(id)))
    }

    pub fn target_id(&self) -> &CacheUpstreamId {
        &self.target_id
    }

    pub fn used_id(&self) -> Option<&CacheUpstreamId> {
        self.used_id.as_ref()
    }
}

impl fmt::Debug for CacheUpstreamProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheUpstreamProvenance")
            .field("has_target", &true)
            .field("has_used", &self.used_id.is_some())
            .finish()
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct CacheKey {
    pub namespace: CacheNamespace,
    /// 由 Cache 模块产生的稳定编码，不包含 DNS ID、原始 client 地址或 runtime revision。
    pub encoded: Arc<[u8]>,
    pub format_version: u16,
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheKey")
            .field("namespace", &self.namespace)
            .field("encoded_len", &self.encoded.len())
            .field("format_version", &self.format_version)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CacheQuality {
    Failure = 0,
    Negative = 1,
    Complete = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheResponseClass {
    NoError,
    NoData,
    NxDomain,
    ServFail,
    Truncated,
}

pub struct CacheEntry {
    pub response: Arc<CanonicalResponse>,
    pub upstream: CacheUpstreamProvenance,
    pub inserted_at: Instant,
    pub expires_at: Instant,
    pub stale_until: Option<Instant>,
    pub response_class: CacheResponseClass,
    pub producer_revision: RuntimeRevision,
    pub quality: CacheQuality,
    pub checksum: u64,
    pub format_version: u16,
}

impl fmt::Debug for CacheEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheEntry")
            .field("upstream", &self.upstream)
            .field("inserted_at", &self.inserted_at)
            .field("expires_at", &self.expires_at)
            .field("stale_until", &self.stale_until)
            .field("response_class", &self.response_class)
            .field("quality", &self.quality)
            .field("checksum", &self.checksum)
            .field("format_version", &self.format_version)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CacheVersion(pub u64);

#[derive(Clone, Debug)]
pub struct CacheRecord {
    pub version: CacheVersion,
    pub entry: Arc<CacheEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheCondition {
    Absent,
    Version(CacheVersion),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheWriteOutcome {
    Inserted(CacheVersion),
    Replaced(CacheVersion),
    Conflict(Option<CacheVersion>),
    RejectedQuality,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheInvalidation {
    Exact(CacheKey),
    Namespace(CacheNamespace),
    Predicate(CacheInvalidationPredicate),
    All,
}

/// 可审计的显式缓存失效条件。
///
/// 条件只能引用 entry 中已有的稳定元数据，不能接受闭包、原始 key 字节或
/// adapter 专属类型。普通 resource refresh 不应构造此条件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheInvalidationPredicate {
    /// 仅移除指定 namespace 中由指定 runtime revision 产生的 entry。
    ProducerRevision {
        namespace: CacheNamespace,
        revision: RuntimeRevision,
    },
}

impl CacheInvalidationPredicate {
    pub fn matches(&self, key: &CacheKey, entry: &CacheEntry) -> bool {
        match self {
            Self::ProducerRevision {
                namespace,
                revision,
            } => namespace == &key.namespace && entry.producer_revision == *revision,
        }
    }
}

impl CacheInvalidation {
    /// 判断一条 record 是否属于显式失效范围。
    ///
    /// Cache adapter 必须使用这一定义实现 predicate，避免各 adapter 对
    /// 同一范围产生不同解释。
    pub fn matches(&self, key: &CacheKey, entry: &CacheEntry) -> bool {
        match self {
            Self::Exact(expected) => expected == key,
            Self::Namespace(namespace) => namespace == &key.namespace,
            Self::Predicate(predicate) => predicate.matches(key, entry),
            Self::All => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStoreStats {
    pub entries: u64,
    pub weighted_size: u64,
    pub hits: u64,
    pub misses: u64,
    pub conflicts: u64,
    pub evictions: u64,
}

/// per-key single-flight 的 leader / follower 分工。
///
/// leader 负责执行真实加载并调用 [`CacheStore::publish_load`] 发布终态；follower 只能
/// 等待相同终态，不能自行触发第二次加载。
#[derive(Debug)]
pub enum CacheLoadReservation {
    Leader(CacheLoadLease),
    Follower(CacheLoadWaiter),
}

pub(crate) trait CacheLoadLeaseReleaser: Send + Sync {
    /// producer 未显式完成 lease 时发布共享失败并唤醒 follower。
    fn abandon(&self, key: &CacheKey, generation: u64);
}

pub(crate) trait CacheLoadWaiterReleaser: Send + Sync {
    /// waiter token 被消费前丢弃时释放其占位。
    fn release(&self, key: &CacheKey, generation: u64);
}

/// 仅由 [`CacheStore::reserve_load`] 产生的加载领导权。
pub struct CacheLoadLease {
    key: CacheKey,
    generation: u64,
    guard: Arc<CacheLoadLeaseGuard>,
}

struct CacheLoadLeaseGuard {
    armed: AtomicBool,
    releaser: Arc<dyn CacheLoadLeaseReleaser>,
}

impl fmt::Debug for CacheLoadLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheLoadLease")
            .field("key", &self.key)
            .field("generation", &self.generation)
            .finish()
    }
}

impl CacheLoadLease {
    pub(crate) fn new(
        key: CacheKey,
        generation: u64,
        releaser: Arc<dyn CacheLoadLeaseReleaser>,
    ) -> Self {
        Self {
            key,
            generation,
            guard: Arc::new(CacheLoadLeaseGuard {
                armed: AtomicBool::new(true),
                releaser,
            }),
        }
    }

    pub(crate) fn key(&self) -> &CacheKey {
        &self.key
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn disarm(&self) {
        self.guard.armed.store(false, Ordering::Release);
    }
}

impl Drop for CacheLoadLease {
    fn drop(&mut self) {
        if self.guard.armed.swap(false, Ordering::AcqRel) {
            self.guard.releaser.abandon(&self.key, self.generation);
        }
    }
}

/// 仅由 [`CacheStore::reserve_load`] 产生的共享结果等待权。
pub struct CacheLoadWaiter {
    key: CacheKey,
    generation: u64,
    guard: Arc<CacheLoadWaiterGuard>,
}

struct CacheLoadWaiterGuard {
    armed: AtomicBool,
    releaser: Arc<dyn CacheLoadWaiterReleaser>,
}

impl fmt::Debug for CacheLoadWaiter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheLoadWaiter")
            .field("key", &self.key)
            .field("generation", &self.generation)
            .finish()
    }
}

impl CacheLoadWaiter {
    pub(crate) fn new(
        key: CacheKey,
        generation: u64,
        releaser: Arc<dyn CacheLoadWaiterReleaser>,
    ) -> Self {
        Self {
            key,
            generation,
            guard: Arc::new(CacheLoadWaiterGuard {
                armed: AtomicBool::new(true),
                releaser,
            }),
        }
    }

    pub(crate) fn key(&self) -> &CacheKey {
        &self.key
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for CacheLoadWaiter {
    fn drop(&mut self) {
        if self.guard.armed.swap(false, Ordering::AcqRel) {
            self.guard.releaser.release(&self.key, self.generation);
        }
    }
}

/// leader 可发布给所有 follower 的稳定加载终态。
#[derive(Clone, Debug)]
pub enum CacheLoadCompletion {
    Ready(CacheRecord),
    Miss,
    Failed(CacheLoadFailure),
}

/// 加载失败的可共享分类，不携带 adapter 原始错误文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheLoadFailure {
    Abandoned,
    Timeout,
    Cancelled(CancelReason),
    Unavailable,
    ResourceExhausted,
    Internal,
}

pub trait CacheStore: Send + Sync {
    fn get<'a>(
        &'a self,
        key: &'a CacheKey,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<Option<CacheRecord>, PortError>>;

    fn compare_and_swap<'a>(
        &'a self,
        key: CacheKey,
        condition: CacheCondition,
        entry: Arc<CacheEntry>,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<CacheWriteOutcome, PortError>>;

    /// 获取一条 per-key single-flight reservation。
    ///
    /// 同一 key 同时只能产生一个 [`CacheLoadReservation::Leader`]。其余调用得到
    /// follower，并通过 [`CacheStore::wait_load`] 等待 leader 的共享终态。follower
    /// 自身的取消或 deadline 仅结束该等待，绝不能取消 leader 或其他 follower 的加载。
    fn reserve_load<'a>(
        &'a self,
        key: CacheKey,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<CacheLoadReservation, PortError>>;

    /// 发布 leader 的最终加载结果并唤醒已登记的 follower。
    ///
    /// 一个 lease 只能发布一次；调用方应在加载成功、未命中和可共享失败时都发布终态，
    /// 以避免 follower 无限等待。
    fn publish_load<'a>(
        &'a self,
        lease: CacheLoadLease,
        completion: CacheLoadCompletion,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<(), PortError>>;

    /// 主加载任务显式放弃 lease。
    ///
    /// adapter 必须向所有 follower 发布 `CacheLoadCompletion::Failed`，并在最后一个
    /// follower 离开后清理占位。lease 自身也带有同步 Drop guard：producer future 被取消
    /// 或 panic 展开时，guard 会触发等价的共享失败释放；这里保留显式 API 供正常错误路径
    /// 使用和未来的 RAII guard 委托。
    fn abandon_load<'a>(
        &'a self,
        lease: CacheLoadLease,
        failure: CacheLoadFailure,
        deadline: Deadline,
    ) -> PortFuture<'a, Result<(), PortError>>;

    /// 等待对应 leader 发布的共享结果。
    ///
    /// `waiter_cancellation` 仅属于当前 waiter；其取消不具有 single-flight 的所有权，
    /// 不得传播给加载任务或其他 waiter。
    ///
    /// waiter token 也带有同步 Drop guard，因此已创建但未完成的 `wait_load` future 被
    /// 取消或丢弃时会释放自己的 follower 占位；该 guard 不会取消共享加载。
    fn wait_load<'a>(
        &'a self,
        waiter: CacheLoadWaiter,
        deadline: Deadline,
        waiter_cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<CacheLoadCompletion, PortError>>;

    fn invalidate(
        &self,
        scope: CacheInvalidation,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<u64, PortError>>;

    fn stats(&self) -> CacheStoreStats;

    fn shutdown(&self, deadline: Deadline) -> PortFuture<'_, Result<(), PortError>>;
}

#[derive(Clone, Debug)]
pub struct PersistentCacheBatch {
    pub records: Vec<(CacheKey, CacheRecord)>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheRecoverySummary {
    pub loaded: u64,
    pub expired: u64,
    pub corrupt: u64,
    pub incompatible: u64,
}

pub trait PersistentCacheStore: Send + Sync {
    fn recover(
        &self,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(PersistentCacheBatch, CacheRecoverySummary), PortError>>;

    fn persist(
        &self,
        batch: PersistentCacheBatch,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(), PortError>>;

    fn maintain_capacity(&self, deadline: Deadline) -> PortFuture<'_, Result<u64, PortError>>;

    fn shutdown(&self, deadline: Deadline) -> PortFuture<'_, Result<(), PortError>>;
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_proto::{
        op::{Message, MessageType, OpCode, Query, ResponseCode},
        rr::{Name, RecordType},
    };

    use crate::dns::{CanonicalQuery, DnsMessageId};

    use super::*;

    fn entry(producer_revision: u64) -> CacheEntry {
        let now = Instant::now();
        let name = Name::from_str("example.com.").expect("test name is valid");
        let question = Query::query(name, RecordType::A);

        let mut query_message = Message::new(1, MessageType::Query, OpCode::Query);
        query_message.add_query(question.clone());
        let query = CanonicalQuery::from_message(query_message).expect("test query is valid");

        let mut response_message = Message::new(2, MessageType::Response, OpCode::Query);
        response_message.metadata.response_code = ResponseCode::NoError;
        response_message.add_query(question);
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(2))
                .expect("test response is valid");

        CacheEntry {
            response: Arc::new(response),
            upstream: CacheUpstreamProvenance::direct_from_validated_config_id("test-upstream")
                .unwrap(),
            inserted_at: now,
            expires_at: now + std::time::Duration::from_secs(60),
            stale_until: None,
            response_class: CacheResponseClass::NoError,
            producer_revision: RuntimeRevision(producer_revision),
            quality: CacheQuality::Complete,
            checksum: 1,
            format_version: CACHE_ENTRY_FORMAT_VERSION,
        }
    }

    fn key(namespace: CacheNamespace) -> CacheKey {
        CacheKey {
            namespace,
            encoded: Arc::from(&b"example.com/A"[..]),
            format_version: 1,
        }
    }

    #[test]
    fn producer_revision_predicate_matches_only_the_exact_entry_revision() {
        let invalidation =
            CacheInvalidation::Predicate(CacheInvalidationPredicate::ProducerRevision {
                namespace: CacheNamespace::Global,
                revision: RuntimeRevision(7),
            });
        let cache_key = key(CacheNamespace::Global);

        assert!(invalidation.matches(&cache_key, &entry(7)));
        assert!(!invalidation.matches(&cache_key, &entry(8)));
    }

    #[test]
    fn predicate_does_not_cross_namespace_boundary() {
        let invalidation =
            CacheInvalidation::Predicate(CacheInvalidationPredicate::ProducerRevision {
                namespace: CacheNamespace::Global,
                revision: RuntimeRevision(u64::MAX),
            });
        let unrelated_namespace = key(CacheNamespace::Strategy(
            CacheStrategyId::from_validated_config_id("secondary").expect("test strategy is valid"),
        ));

        assert!(!invalidation.matches(&unrelated_namespace, &entry(u64::MAX)));
    }

    #[test]
    fn cache_key_debug_does_not_expose_encoded_query_material() {
        let cache_key = CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(&b"private.example.test/A/ecs=198.51.100.0/24"[..]),
            format_version: 1,
        };

        let debug = format!("{cache_key:?}");
        assert!(!debug.contains("private.example.test"));
        assert!(!debug.contains("198.51.100.0"));
        assert!(debug.contains("encoded_len"));
    }

    #[test]
    fn client_strategy_debug_redacts_client_digest_and_strategy_material() {
        let namespace = CacheNamespace::ClientStrategy {
            client_digest: ClientCacheDigest::from_digest([0xA1; 32]),
            strategy: CacheStrategyId::from_validated_config_id("client-private-route")
                .expect("test strategy is valid"),
        };

        let debug = format!("{namespace:?}");
        assert!(!debug.contains("a1"));
        assert!(!debug.contains("client-private-route"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn namespace_identifiers_reject_free_form_material() {
        assert_eq!(
            CacheStrategyId::from_validated_config_id("tenant private route"),
            Err(CacheNamespaceIdError::InvalidCharacter)
        );
    }
}
