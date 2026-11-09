//! PolicyIndex 驱动的 DNS Core 首轮接线。
//!
//! 本 core 先处理已编译的本地 hosts，再执行当前已支持的 hosts/group upstream。
//! 尚未具备真实 connector 的分支保持确定性的 SERVFAIL，不伪造网络结果。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use hickory_proto::{op::ResponseCode, rr::rdata::opt::ClientSubnet};
use ipnet::IpNet;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cache::{
    AnswerEcs, AnswerKey, CacheAdmissionPolicy, CacheCommitCandidate, CacheFacade,
    CacheFacadeOptions, CacheFingerprint, CacheLookup, CacheRefreshPermit, CacheWriteRequest,
    LateCacheFinalizer, MokaCacheStore, build_answer_key, ecs_privacy_prefix,
};
use crate::config::model::EcsMode;
use crate::config::resolve::{
    ConfigId, ResolvedConfig, ResolvedEcs, ResolvedHostsResource, ResolvedTtlOverride,
    ResolvedUpstream, ValueSource,
};
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::policy::{ClientMatch, MatchedRuleKind, PolicyBuildError, PolicyIndex, PolicyRequest};
use crate::ports::PortFuture;
use crate::ports::cache::{
    CacheCondition, CacheKey, CacheLoadCompletion, CacheLoadFailure, CacheLoadReservation,
    CacheQuality, CacheRecord, CacheUpstreamId, CacheUpstreamProvenance, CacheVersion,
};
use crate::ports::exchange::{
    ConnectorId, DnsExchange, TransportFailure, TransportFailureClass, UpstreamOutcome,
};
use crate::ports::storage::StatsSource;
use crate::ports::telemetry::CacheStatus;
use crate::resource::{
    CanonicalDomain, HostsIndex, ResourceLoadError, ResourceSnapshot, ResourceVersion, RuleIndex,
};
use crate::upstream::{
    GroupExecutionResult, GroupMember, GroupSelector, LateResultSink, RegistryError,
    UpstreamAttempt, UpstreamGroupExecutor, UpstreamRegistry,
};

use super::handler::resource_answers;
use super::{
    CanonicalResponse, CoreError, CoreOutcome, DnsCore, DnsCoreCompletion, DnsRequest,
    DnsResolutionObservation,
};

#[derive(Debug, Error)]
pub enum PolicyCoreBuildError {
    #[error("policy index could not be built: {0:?}")]
    Policy(PolicyBuildError),
    #[error("hosts resource `{resource}` could not be loaded: {source}")]
    HostsLoad {
        resource: String,
        #[source]
        source: ResourceLoadError,
    },
    #[error("upstream `{upstream}` could not be built: {reason}")]
    Upstream { upstream: String, reason: String },
    #[error("cache could not be built: {reason}")]
    Cache { reason: String },
}

/// Runtime 级当前 Policy core 指针，供旧 Runtime 的后台刷新读取最新目标。
pub(crate) struct RuntimeCoreCell {
    current: ArcSwapOption<RuntimeCoreTarget>,
}

pub(crate) struct RuntimeCoreTarget {
    pub(crate) core: Arc<PolicyDnsCore>,
    pub(crate) revision: RuntimeRevision,
}

impl Default for RuntimeCoreCell {
    fn default() -> Self {
        Self {
            current: ArcSwapOption::empty(),
        }
    }
}

impl RuntimeCoreCell {
    pub(crate) fn current(&self) -> Option<Arc<RuntimeCoreTarget>> {
        self.current.load_full()
    }

    pub(crate) fn publish(&self, target: Option<Arc<RuntimeCoreTarget>>) {
        self.current.store(target);
    }
}

/// 使用同一份 resolved config 构建 policy/resource 本地回答 core。
#[derive(Clone)]
pub struct PolicyDnsCore {
    policy: Arc<ArcSwap<PolicyState>>,
    upstreams: UpstreamRuntime,
    cache: Arc<CacheFacade>,
    /// 与 `cache` 共用的 Moka store：供快照 owner 遍历，并提供 ECS covering 前缀索引。
    cache_snapshot_source: Arc<MokaCacheStore>,
    late_cache_finalizer: Arc<LateCacheFinalizer>,
    runtime_cell: Arc<ArcSwap<RuntimeCoreCell>>,
    ttl: u32,
}

#[derive(Clone, Debug)]
struct PolicyState {
    index: PolicyIndex,
    host_versions: BTreeMap<ConfigId, ResourceVersion>,
    rule_set_versions: BTreeMap<ConfigId, ResourceVersion>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PolicyResourcePublishError {
    #[error("resource {resource} is not registered in the policy index")]
    UnknownResource { resource: String },
    #[error(
        "resource {resource} candidate version {candidate:?} is not newer than current {current:?}"
    )]
    StaleVersion {
        resource: String,
        current: ResourceVersion,
        candidate: ResourceVersion,
    },
}

impl fmt::Debug for PolicyDnsCore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.policy.load();
        formatter
            .debug_struct("PolicyDnsCore")
            .field("policy", &state.index)
            .field("upstreams", &self.upstreams)
            .field("cache", &self.cache)
            .field("late_cache_finalizer", &self.late_cache_finalizer)
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl PolicyDnsCore {
    pub fn from_config(config: &ResolvedConfig, ttl: u32) -> Result<Self, PolicyCoreBuildError> {
        let direct_upstreams = direct_upstreams(&config.upstreams);
        let registry =
            UpstreamRegistry::from_resolved_with_outbounds(&direct_upstreams, &config.outbounds)
                .map_err(|error| {
                    let error = registry_build_error(error);
                    PolicyCoreBuildError::Upstream {
                        upstream: error.upstream,
                        reason: error.reason,
                    }
                })?;
        Self::from_config_with_registry(config, ttl, registry)
    }

    pub(crate) fn from_config_with_registry(
        config: &ResolvedConfig,
        ttl: u32,
        registry: UpstreamRegistry,
    ) -> Result<Self, PolicyCoreBuildError> {
        let upstreams =
            UpstreamRuntime::from_registry(&config.upstreams, registry).map_err(|error| {
                PolicyCoreBuildError::Upstream {
                    upstream: error.upstream,
                    reason: error.reason,
                }
            })?;
        Self::from_config_with_upstream_runtime(config, ttl, upstreams)
    }

    pub(crate) fn from_config_with_resource_snapshots(
        config: &ResolvedConfig,
        ttl: u32,
        host_snapshots: &BTreeMap<ConfigId, ResourceSnapshot<HostsIndex>>,
        rule_snapshots: &BTreeMap<ConfigId, ResourceSnapshot<RuleIndex>>,
    ) -> Result<Self, PolicyCoreBuildError> {
        let direct_upstreams = direct_upstreams(&config.upstreams);
        let registry =
            UpstreamRegistry::from_resolved_with_outbounds(&direct_upstreams, &config.outbounds)
                .map_err(|error| {
                    let error = registry_build_error(error);
                    PolicyCoreBuildError::Upstream {
                        upstream: error.upstream,
                        reason: error.reason,
                    }
                })?;
        let upstreams =
            UpstreamRuntime::from_registry(&config.upstreams, registry).map_err(|error| {
                PolicyCoreBuildError::Upstream {
                    upstream: error.upstream,
                    reason: error.reason,
                }
            })?;
        Self::from_config_with_upstream_runtime_and_resource_snapshots(
            config,
            ttl,
            upstreams,
            host_snapshots,
            rule_snapshots,
        )
    }

    fn from_config_with_upstream_runtime(
        config: &ResolvedConfig,
        ttl: u32,
        upstreams: UpstreamRuntime,
    ) -> Result<Self, PolicyCoreBuildError> {
        Self::from_config_with_upstream_runtime_and_rule_snapshots(
            config,
            ttl,
            upstreams,
            &BTreeMap::new(),
        )
    }

    fn from_config_with_upstream_runtime_and_rule_snapshots(
        config: &ResolvedConfig,
        ttl: u32,
        upstreams: UpstreamRuntime,
        snapshots: &BTreeMap<ConfigId, ResourceSnapshot<RuleIndex>>,
    ) -> Result<Self, PolicyCoreBuildError> {
        Self::from_config_with_upstream_runtime_and_resource_snapshots(
            config,
            ttl,
            upstreams,
            &BTreeMap::new(),
            snapshots,
        )
    }

    fn from_config_with_upstream_runtime_and_resource_snapshots(
        config: &ResolvedConfig,
        ttl: u32,
        upstreams: UpstreamRuntime,
        host_snapshots: &BTreeMap<ConfigId, ResourceSnapshot<HostsIndex>>,
        rule_snapshots: &BTreeMap<ConfigId, ResourceSnapshot<RuleIndex>>,
    ) -> Result<Self, PolicyCoreBuildError> {
        let host_indexes = host_snapshots
            .iter()
            .map(|(id, snapshot)| (id.clone(), snapshot.compiled_arc()))
            .collect::<BTreeMap<_, _>>();
        let rule_indexes = rule_snapshots
            .iter()
            .map(|(id, snapshot)| (id.clone(), snapshot.compiled_arc()))
            .collect::<BTreeMap<_, _>>();
        let policy =
            PolicyIndex::from_config_with_resource_indexes(config, &host_indexes, &rule_indexes)
                .map_err(PolicyCoreBuildError::Policy)?;
        let (cache, cache_snapshot_source, late_cache_finalizer) = build_cache_facade(config)?;
        let policy_state = PolicyState {
            index: policy,
            host_versions: resource_versions(&config.hosts),
            rule_set_versions: rule_set_versions(&config.rule_sets, rule_snapshots),
        };
        Ok(Self {
            policy: Arc::new(ArcSwap::from_pointee(policy_state)),
            upstreams,
            cache,
            cache_snapshot_source,
            late_cache_finalizer,
            runtime_cell: Arc::new(ArcSwap::from_pointee(RuntimeCoreCell::default())),
            ttl,
        })
    }

    pub fn policy(&self) -> Arc<PolicyIndex> {
        Arc::new(self.policy.load().index.clone())
    }

    pub fn host_resource_count(&self) -> usize {
        self.policy.load().index.host_resource_count()
    }

    pub fn upstream_count(&self) -> usize {
        self.upstreams.len()
    }

    pub fn cache(&self) -> &Arc<CacheFacade> {
        &self.cache
    }

    /// 返回供唯一进程级快照 owner 遍历的生产 Moka handle。
    pub(crate) fn cache_snapshot_source(&self) -> Arc<MokaCacheStore> {
        Arc::clone(&self.cache_snapshot_source)
    }

    /// 返回由 Runtime 生命周期统一托管的 late-cache finalizer。
    pub(crate) fn finalizer_owner(&self) -> Arc<LateCacheFinalizer> {
        Arc::clone(&self.late_cache_finalizer)
    }

    pub(crate) fn attach_runtime_cell(&self, cell: Arc<RuntimeCoreCell>) {
        self.runtime_cell.store(cell);
    }

    fn latest_runtime_target(&self) -> Option<Arc<RuntimeCoreTarget>> {
        self.runtime_cell.load().current()
    }

    pub fn publish_hosts_resource(
        &self,
        snapshot: ResourceSnapshot<HostsIndex>,
    ) -> Result<(), PolicyResourcePublishError> {
        let resource = snapshot.resource_id().clone();
        let candidate = snapshot.version();
        let index = snapshot.compiled().clone();
        loop {
            let current = self.policy.load_full();
            let Some(current_version) = current.host_versions.get(&resource).copied() else {
                return Err(PolicyResourcePublishError::UnknownResource {
                    resource: resource.as_str().to_owned(),
                });
            };
            if candidate <= current_version {
                return Err(PolicyResourcePublishError::StaleVersion {
                    resource: resource.as_str().to_owned(),
                    current: current_version,
                    candidate,
                });
            }
            let index = current
                .index
                .replace_hosts_resource(&resource, index.clone())
                .map_err(|_| PolicyResourcePublishError::UnknownResource {
                    resource: resource.as_str().to_owned(),
                })?;
            let mut host_versions = current.host_versions.clone();
            host_versions.insert(resource.clone(), candidate);
            let next = Arc::new(PolicyState {
                index,
                host_versions,
                rule_set_versions: current.rule_set_versions.clone(),
            });
            let observed = self.policy.compare_and_swap(&current, next);
            if Arc::ptr_eq(&*observed, &current) {
                return Ok(());
            }
        }
    }

    pub fn publish_rule_set_resource(
        &self,
        snapshot: ResourceSnapshot<RuleIndex>,
    ) -> Result<(), PolicyResourcePublishError> {
        let resource = snapshot.resource_id().clone();
        let candidate = snapshot.version();
        let index = snapshot.compiled().clone();
        loop {
            let current = self.policy.load_full();
            let Some(current_version) = current.rule_set_versions.get(&resource).copied() else {
                return Err(PolicyResourcePublishError::UnknownResource {
                    resource: resource.as_str().to_owned(),
                });
            };
            if candidate <= current_version {
                return Err(PolicyResourcePublishError::StaleVersion {
                    resource: resource.as_str().to_owned(),
                    current: current_version,
                    candidate,
                });
            }
            let index = current
                .index
                .replace_rule_set_resource(&resource, index.clone())
                .map_err(|_| PolicyResourcePublishError::UnknownResource {
                    resource: resource.as_str().to_owned(),
                })?;
            let mut rule_set_versions = current.rule_set_versions.clone();
            rule_set_versions.insert(resource.clone(), candidate);
            let next = Arc::new(PolicyState {
                index,
                host_versions: current.host_versions.clone(),
                rule_set_versions,
            });
            let observed = self.policy.compare_and_swap(&current, next);
            if Arc::ptr_eq(&*observed, &current) {
                return Ok(());
            }
        }
    }
}

fn resource_versions(resources: &[ResolvedHostsResource]) -> BTreeMap<ConfigId, ResourceVersion> {
    resources
        .iter()
        .map(|resource| {
            let id = match resource {
                ResolvedHostsResource::Const { id, .. }
                | ResolvedHostsResource::File { id, .. } => id,
            };
            (id.clone(), ResourceVersion::new(1, 1))
        })
        .collect()
}

fn rule_set_versions(
    resources: &[crate::config::resolve::ResolvedRuleSet],
    snapshots: &BTreeMap<ConfigId, ResourceSnapshot<RuleIndex>>,
) -> BTreeMap<ConfigId, ResourceVersion> {
    resources
        .iter()
        .map(|resource| {
            let id = match resource {
                crate::config::resolve::ResolvedRuleSet::Const { id, .. }
                | crate::config::resolve::ResolvedRuleSet::File { id, .. }
                | crate::config::resolve::ResolvedRuleSet::Remote { id, .. } => id,
            };
            (
                id.clone(),
                snapshots
                    .get(id)
                    .map(ResourceSnapshot::version)
                    .unwrap_or_else(|| ResourceVersion::new(1, 1)),
            )
        })
        .collect()
}

fn update_fingerprint_component(hasher: &mut Sha256, component: &[u8]) {
    hasher.update(
        u64::try_from(component.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hasher.update(component);
}

/// 一次完整决策得到的上游查询与答案 key；后台刷新与晚到结果据此复核决策是否仍然一致。
struct PreparedCacheQuery {
    answer: AnswerKey,
    query: super::CanonicalQuery,
    member_queries: Option<Arc<HashMap<ConnectorId, super::CanonicalQuery>>>,
    upstream: ConfigId,
    cache: crate::policy::CacheDecision,
}

struct PolicyLateResultSink {
    core: PolicyDnsCore,
    request: DnsRequest,
    answer: AnswerKey,
    upstream_target_id: CacheUpstreamId,
    producer_revision: crate::dns::RuntimeRevision,
    deadline: Deadline,
}

impl LateResultSink for PolicyLateResultSink {
    fn submit(
        &self,
        query: crate::dns::CanonicalQuery,
        _context: crate::dns::RequestContext,
        attempt: UpstreamAttempt,
    ) {
        let UpstreamAttempt {
            connector,
            outcome: crate::ports::exchange::UpstreamOutcome::Response(response),
            ..
        } = attempt
        else {
            return;
        };
        if !response.matches_query(&query) {
            return;
        }
        let (core, producer_revision) = self.core.latest_runtime_target().map_or_else(
            || (Arc::new(self.core.clone()), self.producer_revision),
            |target| (Arc::clone(&target.core), target.revision),
        );
        let Some(prepared) = core.prepare_cache_query(&self.request) else {
            tracing::debug!(
                operation = "cache_late",
                reason = "cache_ineligible",
                "跳过晚到缓存结果"
            );
            return;
        };
        // 跨 runtime 写入前，重新决策得到的 base、target 与 ECS 维度必须与发起回源时一致。
        if prepared.answer != self.answer {
            tracing::debug!(
                operation = "cache_late",
                reason = "semantics_changed",
                "丢弃旧语义的晚到响应"
            );
            return;
        }
        let cache = Arc::clone(&core.cache);
        let finalizer = Arc::clone(&core.late_cache_finalizer);
        let request = self.request.clone();
        let key = prepared.answer.storage_key(response.edns_client_subnet());
        let answer = prepared.answer;
        let upstream = CacheUpstreamProvenance::new(
            self.upstream_target_id.clone(),
            Some(
                CacheUpstreamId::from_validated_config_id(connector.as_str())
                    .expect("connector ID must be a validated upstream ID"),
            ),
        );
        let deadline = self.deadline;
        let response = Arc::new(response);
        if let Err(error) = finalizer.submit_task(async move {
            // 已过期条目不再提供任何答案，晚到结果无需比较质量即可替换它。
            let (current, superseded) = match cache.lookup(&key, deadline).await {
                Ok(CacheLookup::Fresh(record)) | Ok(CacheLookup::Stale { record, .. }) => {
                    (Some(record), false)
                }
                Ok(CacheLookup::Expired(record)) => (Some(record), true),
                Ok(CacheLookup::Miss) => (None, false),
                other => {
                    tracing::debug!(operation = "cache_late", outcome = ?other, "晚到结果无法读取目标缓存");
                    return;
                }
            };
            let condition = match current {
                None => CacheCondition::Absent,
                // 过期条目仍占用该 key，写回必须按版本替换而不是当作不存在。
                Some(record) if superseded => CacheCondition::Version(record.version),
                Some(record)
                    if late_response_preference(response.class()) > record.entry.quality =>
                {
                    CacheCondition::Version(record.version)
                }
                Some(_) => return,
            };
            if !core
                .prepare_cache_query(&request)
                .is_some_and(|current| current.answer == answer)
            {
                tracing::debug!(operation = "cache_late", reason = "semantics_changed", "放弃晚到响应写回");
                return;
            }
            let outcome = cache
                .write_response(CacheWriteRequest {
                    key,
                    condition,
                    response,
                    upstream,
                    now: Instant::now(),
                    producer_revision,
                    deadline,
                })
                .await;
            tracing::debug!(operation = "cache_late", ?outcome, "晚到缓存写回完成");
        }) {
            tracing::debug!(operation = "cache_late", ?error, "晚到缓存任务未获接纳");
        }
    }

    fn spawn_drain(&self, task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        if let Err(error) = self.core.late_cache_finalizer.submit_task(task) {
            tracing::debug!(
                operation = "cache_late_drain",
                ?error,
                "晚到结果收集任务未获接纳"
            );
        }
    }
}

impl DnsCore for PolicyDnsCore {
    fn resolve<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, Result<CoreOutcome, CoreError>> {
        Box::pin(async move {
            let (result, _, _, cache_commit) = self.resolve_with_metadata(request).await;
            // 直接调用 Policy core 时没有进程级 event worker，仍需完成 lease；服务生产路径
            // 使用 `resolve_with_completion`，因此不会在请求主链等待这次提交。
            if let Some(candidate) = cache_commit {
                let _ = candidate.commit(Duration::from_millis(100)).await;
            }
            result
        })
    }

    fn resolve_with_completion<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, DnsCoreCompletion> {
        Box::pin(async move {
            let (result, observation, cancellation_reason, cache_commit) =
                self.resolve_with_metadata(request).await;
            DnsCoreCompletion {
                result,
                observation,
                cancellation_reason,
                cache_commit,
            }
        })
    }
}

/// 一次答案 lookup 按探测顺序合并的结果。
///
/// 任一 fresh 条目直接作答；否则由调用方从 `stale` 中选第一个仍在当前池乐观窗口内的条目。
/// `seen` 记录所有已存在但未作为 fresh 返回的条目版本：回源写回同一存储 key 时必须按版本 CAS，
/// 过期条目仍占用该 key，当作不存在会与并发写回冲突。
#[derive(Default)]
struct AnswerProbe {
    fresh: Option<CacheRecord>,
    stale: Vec<(CacheRecord, CacheRefreshPermit)>,
    seen: Vec<(CacheKey, CacheVersion)>,
}

impl AnswerProbe {
    /// 写回指定存储 key 时的 CAS 条件。
    fn condition(&self, key: &CacheKey) -> CacheCondition {
        self.seen
            .iter()
            .find(|(seen, _)| seen == key)
            .map_or(CacheCondition::Absent, |(_, version)| {
                CacheCondition::Version(*version)
            })
    }

    /// 回源时对外记录的缓存状态：存在不可作答的条目即为「条目已过期」。
    fn miss_status(&self) -> CacheStatus {
        if self.seen.is_empty() {
            CacheStatus::Miss
        } else {
            CacheStatus::Expired
        }
    }
}

struct PolicyUpstreamResult {
    outcome: PolicyUpstreamOutcome,
    upstream_target_id: Option<Arc<str>>,
    upstream_used_id: Option<Arc<str>>,
    source: StatsSource,
    cache_status: CacheStatus,
    cache_commit: Option<CacheCommitCandidate>,
}

enum PolicyUpstreamOutcome {
    Response(Arc<CanonicalResponse>),
    TransportFailure,
    Cancelled(crate::dns::CancelReason),
}

impl PolicyUpstreamResult {
    /// 将一次真实上游执行结果转换为策略层结果。
    fn upstream(result: UpstreamExecutionResult, cache_status: CacheStatus) -> Self {
        Self {
            outcome: policy_upstream_outcome(result.outcome),
            upstream_target_id: Some(result.target_id),
            upstream_used_id: result.used_id,
            source: StatsSource::Upstream,
            cache_status,
            cache_commit: None,
        }
    }

    /// 保留一次上游执行已归因的 direct upstream 或顶层 group member ID。
    fn upstream_outcome(
        outcome: UpstreamOutcome,
        upstream_target_id: Arc<str>,
        upstream_used_id: Option<Arc<str>>,
        cache_status: CacheStatus,
    ) -> Self {
        Self {
            outcome: policy_upstream_outcome(outcome),
            upstream_target_id: Some(upstream_target_id),
            upstream_used_id,
            source: StatsSource::Upstream,
            cache_status,
            cache_commit: None,
        }
    }

    /// 从 cache entry 恢复产生该响应的 target 与实际 direct/member。
    fn cache(
        outcome: UpstreamOutcome,
        record: &crate::ports::cache::CacheRecord,
        cache_status: CacheStatus,
    ) -> Self {
        Self {
            outcome: policy_upstream_outcome(outcome),
            upstream_target_id: Some(Arc::from(record.entry.upstream.target_id().as_str())),
            upstream_used_id: record
                .entry
                .upstream
                .used_id()
                .map(|id| Arc::from(id.as_str())),
            source: StatsSource::Cache,
            cache_status,
            cache_commit: None,
        }
    }
}

fn policy_upstream_outcome(outcome: UpstreamOutcome) -> PolicyUpstreamOutcome {
    match outcome {
        UpstreamOutcome::Response(response) => PolicyUpstreamOutcome::Response(Arc::new(response)),
        UpstreamOutcome::TransportFailure(_) => PolicyUpstreamOutcome::TransportFailure,
        UpstreamOutcome::Cancelled(reason) => PolicyUpstreamOutcome::Cancelled(reason),
    }
}

/// 从已校验的策略与 connector ID 构造可持久化的缓存来源。
fn cache_upstream_provenance(target_id: &str, used_id: Option<&str>) -> CacheUpstreamProvenance {
    let target_id = CacheUpstreamId::from_validated_config_id(target_id)
        .expect("resolved upstream target must be valid cache provenance");
    let used_id = used_id.map(|used_id| {
        CacheUpstreamId::from_validated_config_id(used_id)
            .expect("selected connector must be valid cache provenance")
    });
    CacheUpstreamProvenance::new(target_id, used_id)
}

impl PolicyDnsCore {
    async fn resolve_with_metadata(
        &self,
        request: &DnsRequest,
    ) -> (
        Result<CoreOutcome, CoreError>,
        Option<DnsResolutionObservation>,
        Option<crate::dns::CancelReason>,
        Option<CacheCommitCandidate>,
    ) {
        let meta = &request.context.meta;
        if meta.cancellation.is_cancelled() || meta.deadline.is_expired(Instant::now()) {
            return (
                Ok(CoreOutcome::NoResponse),
                None,
                meta.cancellation.reason(),
                None,
            );
        }

        let Some(listener_id) = ConfigId::new(meta.listener_id.as_ref().to_owned()).ok() else {
            return (servfail(request), None, None, None);
        };
        let qname = match CanonicalDomain::parse(&request.query.question().name().to_ascii()) {
            Ok(qname) => qname,
            Err(_) => return (servfail(request), None, None, None),
        };
        let policy = self.policy.load();
        let policy_request = PolicyRequest {
            listener_id: &listener_id,
            doh_route_id: request.context.meta.route_id.as_ref(),
            client_id: request
                .context
                .client
                .client_id
                .as_ref()
                .map(|client_id| client_id.as_str()),
            client_addr: request.context.client.client_addr,
            client_digest: None,
            qname: Some(&qname),
        };
        let context = match policy.index.prepare_context(&policy_request) {
            Ok(context) => context,
            Err(_error) => return (servfail(request), None, None, None),
        };
        let client_match = context.client.observation(
            request
                .context
                .client
                .client_id
                .as_ref()
                .map(|client_id| client_id.as_str()),
        );
        let client_bucket = match &context.client {
            ClientMatch::Matched { client, .. } => Some(Arc::from(client.name.as_str())),
            ClientMatch::Unknown => None,
        };
        let strategy_id = Some(Arc::from(context.strategy.id.as_str()));
        let decision = match policy.index.evaluate_route(&context, Some(&qname)) {
            Ok(decision) => decision,
            Err(_error) => return (servfail(request), None, None, None),
        };
        let plan = context.into_resolution_plan(decision);
        let matched_rule = matched_rule_observation(&policy, plan.matched_rule.as_ref());

        if let Some(resource_id) = plan.hosts {
            let Some(index) = policy.index.hosts_index(&resource_id) else {
                return (servfail(request), None, None, None);
            };
            let (answers, known_name) = resource_answers(
                std::slice::from_ref(index.as_ref()),
                request.query.question().name(),
                request.query.question().query_type(),
                self.ttl,
            );
            let code = if answers.is_empty() && !known_name {
                ResponseCode::NXDomain
            } else {
                ResponseCode::NoError
            };
            let response = if code == ResponseCode::NoError && !answers.is_empty() {
                CanonicalResponse::response_with_answers(&request.query, answers)
            } else {
                CanonicalResponse::response_with_code(&request.query, code, answers)
            };
            let result = response
                .map(|mut response| {
                    apply_ttl_override(&mut response, &plan.ttl_override);
                    CoreOutcome::Response(Arc::new(response))
                })
                .map_err(CoreError::ResponseConstruction);
            return (
                result,
                Some(DnsResolutionObservation {
                    client_match,
                    client_bucket,
                    strategy_id,
                    matched_rule,
                    upstream_id: None,
                    upstream_member_id: None,
                    upstream_used_id: None,
                    source: StatsSource::Hosts,
                    cache_status: CacheStatus::Disabled,
                }),
                None,
                None,
            );
        }

        let Some(outcome) = self.resolve_upstream(request, &plan).await else {
            return (
                servfail(request),
                Some(DnsResolutionObservation {
                    client_match,
                    client_bucket,
                    strategy_id,
                    matched_rule: matched_rule.clone(),
                    upstream_id: Some(Arc::from(plan.upstream.as_str())),
                    upstream_member_id: None,
                    upstream_used_id: None,
                    source: StatsSource::Upstream,
                    cache_status: CacheStatus::StoreUnavailable,
                }),
                None,
                None,
            );
        };
        let upstream_id = outcome.upstream_target_id.clone();
        let upstream_used_id = outcome.upstream_used_id.clone();
        let upstream_member_id = upstream_used_id
            .clone()
            .filter(|selected| upstream_id.as_deref() != Some(selected.as_ref()));
        let cancellation_reason = match &outcome.outcome {
            PolicyUpstreamOutcome::Cancelled(reason) => Some(*reason),
            PolicyUpstreamOutcome::Response(_) | PolicyUpstreamOutcome::TransportFailure => None,
        };
        let result = match outcome.outcome {
            PolicyUpstreamOutcome::Response(mut response)
                if response.matches_query(&request.query) =>
            {
                if let Some(ttl) = response.short_cache_ttl() {
                    if outcome.source != StatsSource::Cache {
                        // 缓存命中已按剩余寿命改写，不能在输出阶段重新补满 10s。
                        Arc::make_mut(&mut response).rewrite_short_cache_ttl(ttl);
                    }
                } else {
                    if response.is_negative()
                        && let Some(max) = self.negative_ttl_max()
                    {
                        // 只压客户端可见 TTL；commit candidate 保留 origin response。
                        Arc::make_mut(&mut response).cap_negative_ttl(max);
                    }
                    apply_ttl_override(Arc::make_mut(&mut response), &plan.ttl_override);
                }
                Ok(CoreOutcome::Response(response))
            }
            PolicyUpstreamOutcome::Cancelled(_) => Ok(CoreOutcome::NoResponse),
            PolicyUpstreamOutcome::Response(_) | PolicyUpstreamOutcome::TransportFailure => {
                servfail(request)
            }
        };
        (
            result,
            Some(DnsResolutionObservation {
                client_match,
                client_bucket,
                strategy_id,
                matched_rule,
                upstream_id,
                upstream_member_id,
                upstream_used_id,
                source: outcome.source,
                cache_status: outcome.cache_status,
            }),
            cancellation_reason,
            outcome.cache_commit,
        )
    }

    async fn resolve_upstream(
        &self,
        request: &DnsRequest,
        plan: &crate::policy::ResolutionPlan,
    ) -> Option<PolicyUpstreamResult> {
        let (prepared, answer) = self.prepare_upstream(plan, request);
        let PreparedUpstreamQuery {
            query,
            member_queries,
            ..
        } = prepared;
        let Some(answer) = answer else {
            let reason = if plan.cache.is_enabled() {
                "key_unavailable"
            } else {
                "cache_disabled"
            };
            tracing::debug!(
                strategy = plan.strategy.id.as_str(),
                upstream = plan.upstream.as_str(),
                reason,
                "绕过响应缓存"
            );
            return self
                .upstreams
                .exchange(
                    &plan.upstream,
                    &query,
                    &request.context,
                    None,
                    member_queries,
                )
                .await
                .map(|outcome| PolicyUpstreamResult::upstream(outcome, CacheStatus::Disabled));
        };
        let deadline = request.context.meta.deadline;
        let probe = self.probe_answer(&answer, &plan.cache, deadline).await;
        if let Some(record) = &probe.fresh {
            return Some(PolicyUpstreamResult::cache(
                UpstreamOutcome::Response(self.fresh_cache_response(record)),
                record,
                CacheStatus::Fresh,
            ));
        }
        let now = Instant::now();
        if let Some((record, refresh, answer_ttl)) =
            probe.stale.iter().find_map(|(record, refresh)| {
                stale_answer_ttl(&plan.cache, &record.entry, now)
                    .map(|answer_ttl| (record, refresh, answer_ttl))
            })
        {
            let mut stale_response = (*record.entry.response).clone();
            stale_response.set_ttl(answer_ttl);
            if refresh.try_consume() {
                self.schedule_optimistic_refresh(request);
            } else {
                request
                    .context
                    .meta
                    .completion
                    .begin_cache(crate::dns::CacheActivityKind::Refresh)
                    .finish(crate::dns::CacheActivityOutcome::Coalesced);
            }
            return Some(PolicyUpstreamResult::cache(
                UpstreamOutcome::Response(stale_response),
                record,
                CacheStatus::Stale,
            ));
        }
        // 乐观窗口已结束或没有条目：本次必须回源，状态区分「条目已过期」与「从未缓存」。
        let miss_status = probe.miss_status();

        let late_sink = self.late_result_sink(&answer, request, &plan.upstream);
        let reservation = match self.cache.reserve_load(answer.load_key(), deadline).await {
            Ok(reservation) => reservation,
            Err(_) => {
                return self
                    .upstreams
                    .exchange(
                        &plan.upstream,
                        &query,
                        &request.context,
                        Some(Arc::clone(&late_sink)),
                        member_queries.clone(),
                    )
                    .await
                    .map(|outcome| {
                        PolicyUpstreamResult::upstream(outcome, CacheStatus::StoreUnavailable)
                    });
            }
        };
        match reservation {
            CacheLoadReservation::Follower(waiter) => {
                match self
                    .cache
                    .wait_load(waiter, deadline, &request.context.meta.cancellation)
                    .await
                {
                    Ok(CacheLoadCompletion::Ready(record)) => Some(PolicyUpstreamResult::cache(
                        UpstreamOutcome::Response(self.fresh_cache_response(&record)),
                        &record,
                        CacheStatus::Fresh,
                    )),
                    Ok(CacheLoadCompletion::Failed(CacheLoadFailure::Cancelled(reason))) => {
                        Some(PolicyUpstreamResult::upstream_outcome(
                            UpstreamOutcome::Cancelled(reason),
                            Arc::from(plan.upstream.as_str()),
                            None,
                            miss_status,
                        ))
                    }
                    Ok(CacheLoadCompletion::Miss) | Ok(CacheLoadCompletion::Failed(_)) | Err(_) => {
                        self.upstreams
                            .exchange(
                                &plan.upstream,
                                &query,
                                &request.context,
                                Some(Arc::clone(&late_sink)),
                                member_queries.clone(),
                            )
                            .await
                            .map(|outcome| PolicyUpstreamResult::upstream(outcome, miss_status))
                    }
                }
            }
            CacheLoadReservation::Leader(lease) => {
                let Some(outcome) = self
                    .upstreams
                    .exchange(
                        &plan.upstream,
                        &query,
                        &request.context,
                        Some(Arc::clone(&late_sink)),
                        member_queries,
                    )
                    .await
                else {
                    drop(lease);
                    return None;
                };
                let UpstreamExecutionResult {
                    outcome,
                    target_id,
                    used_id,
                } = outcome;
                match outcome {
                    UpstreamOutcome::Response(response) => {
                        if !response.matches_query(&query) {
                            drop(lease);
                            return Some(PolicyUpstreamResult::upstream_outcome(
                                UpstreamOutcome::Response(response),
                                target_id,
                                used_id,
                                miss_status,
                            ));
                        }
                        // single-flight 按查询 ECS 合并；存储 key 要等应答 scope 确定后才能计算。
                        let key = answer.storage_key(response.edns_client_subnet());
                        let condition = probe.condition(&key);
                        let response = Arc::new(response);
                        let cache_commit = CacheCommitCandidate::new(
                            Arc::clone(&self.cache),
                            CacheWriteRequest {
                                key,
                                condition,
                                response: Arc::clone(&response),
                                upstream: cache_upstream_provenance(
                                    target_id.as_ref(),
                                    used_id.as_deref(),
                                ),
                                now: Instant::now(),
                                producer_revision: request.context.runtime_revision,
                                deadline,
                            },
                            lease,
                        );
                        Some(PolicyUpstreamResult {
                            outcome: PolicyUpstreamOutcome::Response(response),
                            upstream_target_id: Some(target_id),
                            upstream_used_id: used_id,
                            source: StatsSource::Upstream,
                            cache_status: miss_status,
                            cache_commit: Some(cache_commit),
                        })
                    }
                    UpstreamOutcome::Cancelled(reason) => {
                        drop(lease);
                        Some(PolicyUpstreamResult::upstream_outcome(
                            UpstreamOutcome::Cancelled(reason),
                            target_id,
                            used_id,
                            miss_status,
                        ))
                    }
                    UpstreamOutcome::TransportFailure(failure) => {
                        drop(lease);
                        Some(PolicyUpstreamResult::upstream_outcome(
                            UpstreamOutcome::TransportFailure(failure),
                            target_id,
                            used_id,
                            miss_status,
                        ))
                    }
                }
            }
        }
    }

    /// 按完整决策生成最终上游查询；当前缓存池启用时同时生成答案 key。
    fn prepare_upstream(
        &self,
        plan: &crate::policy::ResolutionPlan,
        request: &DnsRequest,
    ) -> (PreparedUpstreamQuery, Option<AnswerKey>) {
        let prepared = self.upstreams.prepare_query(
            &plan.upstream,
            &request.query,
            &plan.edns_client_subnet,
            request.context.client.client_addr,
        );
        let answer = plan
            .cache
            .namespace()
            .zip(self.upstreams.target_fingerprints.get(&plan.upstream))
            .and_then(|(namespace, target)| {
                build_answer_key(
                    namespace.clone(),
                    &prepared.query,
                    request.context.transport.cache_compatibility,
                    *target,
                    prepared.ecs,
                )
                .ok()
            });
        (prepared, answer)
    }

    /// 按最长前缀选择当前池可返回的答案；继续收集旧版本供 scope 迁移时 CAS。
    async fn probe_answer(
        &self,
        answer: &AnswerKey,
        cache: &crate::policy::CacheDecision,
        deadline: Deadline,
    ) -> AnswerProbe {
        let mut probe = AnswerProbe::default();
        let covering = self.cache_snapshot_source.covering_prefixes(answer);
        for key in answer.probe_keys(&covering) {
            match self.cache.lookup(&key, deadline).await {
                Ok(CacheLookup::Fresh(record)) => {
                    if probe.stale.is_empty() {
                        probe.fresh = Some(record);
                    }
                    break;
                }
                Ok(CacheLookup::Stale { record, refresh }) => {
                    probe.seen.push((key, record.version));
                    if probe.stale.is_empty()
                        && stale_answer_ttl(cache, &record.entry, Instant::now()).is_some()
                    {
                        probe.stale.push((record, refresh));
                    }
                }
                Ok(CacheLookup::Expired(record)) => probe.seen.push((key, record.version)),
                Ok(CacheLookup::Disabled | CacheLookup::Miss | CacheLookup::StoreUnavailable)
                | Err(_) => {}
            }
        }
        probe
    }

    /// 重新执行当前策略/资源决策，得到最新的上游查询与答案 key；本地 hosts 回答或缓存关闭时为空。
    fn prepare_cache_query(&self, request: &DnsRequest) -> Option<PreparedCacheQuery> {
        let listener_id =
            ConfigId::new(request.context.meta.listener_id.as_ref().to_owned()).ok()?;
        let qname = CanonicalDomain::parse(&request.query.question().name().to_ascii()).ok()?;
        let policy = self.policy.load();
        let context = policy
            .index
            .prepare_context(&PolicyRequest {
                listener_id: &listener_id,
                doh_route_id: request.context.meta.route_id.as_ref(),
                client_id: request
                    .context
                    .client
                    .client_id
                    .as_ref()
                    .map(|id| id.as_str()),
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .ok()?;
        let decision = policy.index.evaluate_route(&context, Some(&qname)).ok()?;
        if decision.hosts.is_some() {
            return None;
        }
        let plan = context.into_resolution_plan(decision);
        let (prepared, answer) = self.prepare_upstream(&plan, request);
        Some(PreparedCacheQuery {
            answer: answer?,
            query: prepared.query,
            member_queries: prepared.member_queries,
            cache: plan.cache,
            upstream: plan.upstream,
        })
    }

    /// 当前运行时的负缓存 TTL 上限；来自 `dns.cache.negative_ttl_max`。
    fn negative_ttl_max(&self) -> Option<Duration> {
        self.cache
            .options()
            .admission
            .negative_ttl_max
            .filter(|max| !max.is_zero())
    }

    /// 以当前负缓存上限构造 fresh 命中响应。
    fn fresh_cache_response(&self, record: &crate::ports::cache::CacheRecord) -> CanonicalResponse {
        fresh_cache_response(record, self.negative_ttl_max())
    }

    /// 在最新 runtime 中重新决策并回源刷新 stale 条目。
    ///
    /// 写回 key 由刷新应答的 scope 决定，CAS 条件取自目标 store 中刷新前实际读到的版本；
    /// 跨 runtime 时目标 store 若已有 fresh 答案则不回源。
    fn schedule_optimistic_refresh(&self, request: &DnsRequest) {
        let (core, producer_revision) = self.latest_runtime_target().map_or_else(
            || (Arc::new(self.clone()), request.context.runtime_revision),
            |target| (Arc::clone(&target.core), target.revision),
        );
        let finalizer = Arc::clone(&core.late_cache_finalizer);
        let mut request = request.clone();
        request.context = optimistic_refresh_context(&request.context);
        request.context.runtime_revision = producer_revision;
        let refresh_observation = request
            .context
            .meta
            .completion
            .begin_cache(crate::dns::CacheActivityKind::Refresh);
        if let Err(error) = finalizer.submit_task(async move {
            let Some(prepared) = core.prepare_cache_query(&request) else {
                tracing::debug!(
                    operation = "cache_refresh",
                    reason = "cache_ineligible",
                    "跳过缓存刷新"
                );
                refresh_observation.finish(crate::dns::CacheActivityOutcome::Skipped);
                return;
            };
            let deadline = request.context.meta.deadline;
            let probe = core
                .probe_answer(&prepared.answer, &prepared.cache, deadline)
                .await;
            if probe.fresh.is_some() {
                tracing::debug!(
                    operation = "cache_refresh",
                    reason = "already_fresh",
                    "目标缓存已更新"
                );
                refresh_observation.finish(crate::dns::CacheActivityOutcome::Skipped);
                return;
            }
            let before_refresh = core.cache_snapshot_source.snapshot_generation();
            let Some(UpstreamExecutionResult {
                outcome: UpstreamOutcome::Response(response),
                target_id,
                used_id,
            }) = core
                .upstreams
                .exchange(
                    &prepared.upstream,
                    &prepared.query,
                    &request.context,
                    None,
                    prepared.member_queries,
                )
                .await
            else {
                tracing::debug!(
                    operation = "cache_refresh",
                    reason = "upstream_failed",
                    "缓存刷新未取得响应"
                );
                refresh_observation.finish(crate::dns::CacheActivityOutcome::Failed);
                return;
            };
            if !response.matches_query(&prepared.query) {
                tracing::debug!(
                    operation = "cache_refresh",
                    reason = "question_mismatch",
                    "拒绝不匹配的刷新响应"
                );
                refresh_observation.finish(crate::dns::CacheActivityOutcome::Rejected);
                return;
            }
            if !core
                .prepare_cache_query(&request)
                .is_some_and(|current| current.answer == prepared.answer)
            {
                tracing::debug!(
                    operation = "cache_refresh",
                    reason = "semantics_changed",
                    "放弃旧语义的刷新响应"
                );
                refresh_observation.finish(crate::dns::CacheActivityOutcome::Skipped);
                return;
            }
            refresh_observation.route(Some(target_id.as_ref()), used_id.as_deref());
            let class = response.class();
            let key = prepared.answer.storage_key(response.edns_client_subnet());
            let outcome = core
                .cache
                .write_response(CacheWriteRequest {
                    condition: probe.condition(&key),
                    key: key.clone(),
                    response: Arc::new(response),
                    upstream: cache_upstream_provenance(target_id.as_ref(), used_id.as_deref()),
                    now: Instant::now(),
                    producer_revision,
                    deadline,
                })
                .await;
            if let Ok(crate::cache::CacheWriteResult::Stored {
                outcome:
                    crate::ports::cache::CacheWriteOutcome::Inserted(version)
                    | crate::ports::cache::CacheWriteOutcome::Replaced(version),
                ..
            }) = &outcome
                && let Err(error) =
                    core.cache_snapshot_source
                        .invalidate_covered(&key, *version, before_refresh)
            {
                tracing::debug!(operation = "cache_refresh", ?error, "清理旧 scope 条目失败");
            }
            let ttl_secs = crate::cache::cache_written_ttl_secs(&outcome);
            tracing::debug!(
                operation = "cache_refresh",
                ?class,
                ?ttl_secs,
                ?outcome,
                "缓存刷新写回完成"
            );
            refresh_observation.result(class.into(), ttl_secs);
            refresh_observation.finish(crate::cache::cache_activity_outcome(&outcome));
        }) {
            tracing::debug!(operation = "cache_refresh", ?error, "缓存刷新任务未获接纳");
        }
    }

    fn late_result_sink(
        &self,
        answer: &AnswerKey,
        request: &DnsRequest,
        upstream: &ConfigId,
    ) -> Arc<dyn LateResultSink> {
        Arc::new(PolicyLateResultSink {
            core: self.clone(),
            request: request.clone(),
            answer: answer.clone(),
            upstream_target_id: CacheUpstreamId::from_validated_config_id(upstream.as_str())
                .expect("resolved upstream ID must be valid cache provenance"),
            producer_revision: request.context.runtime_revision,
            deadline: Deadline::new(
                Instant::now() + Duration::from_secs(OPTIMISTIC_REFRESH_TIMEOUT_SECS),
            ),
        })
    }
}

/// 将 Policy 内部 rule 结果压缩为不含规则文本和 matcher 内容的观测摘要。
fn matched_rule_observation(
    policy: &PolicyState,
    matched: Option<&crate::policy::MatchedRule>,
) -> Option<super::MatchedRuleObservation> {
    let matched = matched?;
    let (source, resource_id, resource_version, ordinal) = match &matched.kind {
        MatchedRuleKind::ListenerHosts { resource } => (
            super::MatchedRuleSource::ListenerHosts,
            resource,
            policy.host_versions.get(resource).copied(),
            None,
        ),
        MatchedRuleKind::Hosts { resource } => (
            super::MatchedRuleSource::StrategyHosts,
            resource,
            policy.host_versions.get(resource).copied(),
            Some(u64::try_from(matched.ordinal).expect("rule ordinal must fit u64")),
        ),
        MatchedRuleKind::RuleSet { resource, .. } => (
            super::MatchedRuleSource::RuleSet,
            resource,
            policy.rule_set_versions.get(resource).copied(),
            Some(u64::try_from(matched.ordinal).expect("rule ordinal must fit u64")),
        ),
    };
    Some(super::MatchedRuleObservation {
        source,
        resource_id: Arc::from(resource_id.as_str()),
        resource_version,
        ordinal,
    })
}

/// 在响应离开 Policy Core 前应用当前请求选中的 TTL 覆写。
///
/// 缓存写入发生在此步骤之前，因此这里仅改变客户端可见 TTL，不改变缓存过期时间。
fn apply_ttl_override(response: &mut CanonicalResponse, ttl_override: &ResolvedTtlOverride) {
    if ttl_override.enabled {
        response.clamp_ttl(ttl_override.min, ttl_override.max);
    }
}

/// 从 fresh cache record 构造客户端响应，并按已缓存时间递减 RR TTL。
///
/// 短期响应直接使用剩余寿命并同步 SOA.MINIMUM；其余负应答仍按配置上限截断。
/// 不改写缓存中的 origin response，避免污染 checksum 或客户端之间共享的响应。
fn fresh_cache_response(
    record: &crate::ports::cache::CacheRecord,
    negative_ttl_max: Option<Duration>,
) -> CanonicalResponse {
    let now = Instant::now();
    let mut response = (*record.entry.response).clone();
    if let Some(ttl) = response.short_cache_ttl() {
        response.rewrite_short_cache_ttl(
            record
                .entry
                .expires_at
                .saturating_duration_since(now)
                .min(ttl),
        );
        return response;
    }
    response.age_ttl(now.saturating_duration_since(record.entry.inserted_at));
    if response.is_negative() {
        let remaining = record.entry.expires_at.saturating_duration_since(now);
        let cap = negative_ttl_max.map_or(remaining, |max| remaining.min(max));
        response.cap_negative_ttl(cap);
    }
    response
}

/// 仅在当前 pool 对该响应质量允许的 optimistic max-age 内返回其 stale answer TTL。
///
/// REFUSED/空 Answer 到期即回源；其余应答按质量使用当前池窗口，Failure 永不乐观返回。
/// 即使条目携带旧的长 stale_until，也不能绕过短期响应规则。
fn stale_answer_ttl(
    decision: &crate::policy::CacheDecision,
    entry: &crate::ports::cache::CacheEntry,
    now: Instant,
) -> Option<Duration> {
    if entry.response.short_cache_ttl().is_some() {
        return None;
    }
    let answer_ttl = decision.optimistic_answer_ttl()?;
    let max_age = match entry.quality {
        CacheQuality::Complete => decision.optimistic_max_age()?,
        CacheQuality::Negative => decision.optimistic_negative_max_age()?,
        CacheQuality::Failure => return None,
    };
    if max_age.is_zero() {
        return None;
    }
    entry
        .expires_at
        .checked_add(max_age)
        .is_some_and(|stale_until| now < stale_until)
        .then_some(answer_ttl)
}

/// 根据请求级 ECS 配置生成真正发往上游的 canonical query。
fn effective_upstream_query(
    query: &crate::dns::CanonicalQuery,
    ecs: &ResolvedEcs,
    client_addr: Option<IpAddr>,
) -> crate::dns::CanonicalQuery {
    let subnet = match ecs.mode {
        EcsMode::Disabled => None,
        EcsMode::Custom => ecs
            .custom_ip
            .map(|network| ClientSubnet::new(network.network(), network.prefix_len(), 0)),
        EcsMode::Client => query
            .edns_client_subnet()
            .and_then(normalize_client_subnet)
            .or_else(|| client_addr.map(client_address_subnet)),
    };
    query.with_edns_client_subnet(subnet)
}

/// 校验并规范化客户端自带的 ECS：source 截断到隐私上限（IPv4 `/24`、IPv6 `/56`），
/// 清除 host bits，查询 scope 按 RFC 7871 置 0。
fn normalize_client_subnet(subnet: ClientSubnet) -> Option<ClientSubnet> {
    let max_prefix = match subnet.addr() {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    if subnet.source_prefix() > max_prefix || subnet.scope_prefix() > max_prefix {
        return None;
    }
    let source_prefix = subnet
        .source_prefix()
        .min(ecs_privacy_prefix(subnet.addr()));
    let network = IpNet::new(subnet.addr(), source_prefix).ok()?;
    Some(ClientSubnet::new(network.network(), source_prefix, 0))
}

/// 按固定的隐私前缀从客户端地址生成 ECS，避免传递完整客户端地址。
fn client_address_subnet(address: IpAddr) -> ClientSubnet {
    let source_prefix = ecs_privacy_prefix(address);
    let network = IpNet::new(address, source_prefix).expect("fixed ECS prefix must be valid");
    ClientSubnet::new(network.network(), source_prefix, 0)
}

fn late_response_preference(class: crate::dns::ResponseClass) -> CacheQuality {
    match class {
        crate::dns::ResponseClass::Positive => CacheQuality::Complete,
        crate::dns::ResponseClass::NoData | crate::dns::ResponseClass::NxDomain => {
            CacheQuality::Negative
        }
        crate::dns::ResponseClass::Refused
        | crate::dns::ResponseClass::ServFail
        | crate::dns::ResponseClass::Truncated
        | crate::dns::ResponseClass::Other(_) => CacheQuality::Failure,
    }
}

const DEFAULT_LATE_CACHE_FINALIZER_CAPACITY: usize = 64;
const OPTIMISTIC_REFRESH_TIMEOUT_SECS: u64 = 2;
type CacheAssembly = (
    Arc<CacheFacade>,
    Arc<MokaCacheStore>,
    Arc<LateCacheFinalizer>,
);

fn build_cache_facade(config: &ResolvedConfig) -> Result<CacheAssembly, PolicyCoreBuildError> {
    let store = MokaCacheStore::with_max_weight(config.dns.cache.memory_max_size_bytes).map_err(
        |error| PolicyCoreBuildError::Cache {
            reason: error.to_string(),
        },
    )?;
    let options = cache_runtime_options(config);
    let finalizer =
        LateCacheFinalizer::new(DEFAULT_LATE_CACHE_FINALIZER_CAPACITY).map_err(|error| {
            PolicyCoreBuildError::Cache {
                reason: format!("{error:?}"),
            }
        })?;
    let store = Arc::new(store);
    Ok((
        Arc::new(CacheFacade::new(store.clone(), options)),
        store,
        Arc::new(finalizer),
    ))
}

/// 汇总所有逻辑 pool 的运行时能力；`dns.cache.enabled` 只控制全局池。
///
/// 完整应答与负应答的乐观窗口分别取各启用池的最大值写入条目，实际返回再由当前池收紧。
fn cache_runtime_options(config: &ResolvedConfig) -> CacheFacadeOptions {
    let mut enabled = config.dns.cache.enabled;
    let global_optimistic = (config.dns.cache.enabled && config.dns.cache.optimistic.enabled)
        .then_some(&config.dns.cache.optimistic);
    let mut optimistic_max_age = global_optimistic.map(|optimistic| optimistic.max_age);
    let mut negative_max_age = global_optimistic.map(|optimistic| optimistic.negative_max_age);
    for cache in config
        .strategies
        .iter()
        .filter_map(|strategy| strategy.cache.as_ref())
        .chain(
            config
                .clients
                .iter()
                .filter_map(|client| client.cache.as_ref()),
        )
        .filter(|cache| cache.enabled)
    {
        enabled = true;
        if let Some(optimistic) = cache
            .optimistic
            .as_ref()
            .filter(|optimistic| optimistic.enabled)
        {
            optimistic_max_age = Some(optimistic_max_age.map_or(optimistic.max_age, |current| {
                current.max(optimistic.max_age)
            }));
            negative_max_age = Some(
                negative_max_age.map_or(optimistic.negative_max_age, |current| {
                    current.max(optimistic.negative_max_age)
                }),
            );
        }
    }
    CacheFacadeOptions {
        enabled,
        optimistic_enabled: optimistic_max_age.is_some(),
        admission: CacheAdmissionPolicy::new(config.dns.cache.failure_ttl, optimistic_max_age)
            .with_negative(Some(config.dns.cache.negative_ttl_max), negative_max_age),
    }
}

fn optimistic_refresh_context(context: &crate::dns::RequestContext) -> crate::dns::RequestContext {
    let mut refresh = context.clone();
    refresh.meta.deadline = Deadline::new(
        Instant::now() + std::time::Duration::from_secs(OPTIMISTIC_REFRESH_TIMEOUT_SECS),
    );
    refresh.meta.cancellation = Cancellation::new();
    refresh
}

/// 测试辅助：不带 ECS 的上游应答会写入的存储 key。
#[cfg(test)]
fn cache_key(
    core: &PolicyDnsCore,
    _plan: &crate::policy::ResolutionPlan,
    request: &DnsRequest,
) -> Option<CacheKey> {
    core.prepare_cache_query(request)
        .map(|prepared| prepared.answer.storage_key(None))
}

/// 测试辅助：当前决策得到的答案 key。
#[cfg(test)]
fn answer_key(core: &PolicyDnsCore, request: &DnsRequest) -> Option<AnswerKey> {
    core.prepare_cache_query(request)
        .map(|prepared| prepared.answer)
}

#[derive(Clone)]
struct UpstreamRuntime {
    direct: BTreeMap<ConfigId, Arc<dyn DnsExchange>>,
    groups: BTreeMap<ConfigId, Arc<UpstreamGroupExecutor>>,
    all: BTreeMap<ConfigId, Arc<dyn DnsExchange>>,
    group_member_ecs: BTreeMap<ConfigId, Arc<BTreeMap<ConfigId, Option<ResolvedEcs>>>>,
    /// 每个 target 子树的内容摘要；只随该子树自身的配置变化。
    target_fingerprints: BTreeMap<ConfigId, CacheFingerprint>,
}

/// 发往 target 的最终查询；成员最终 ECS 不同时保留逐 connector 查询。
struct PreparedUpstreamQuery {
    query: super::CanonicalQuery,
    member_queries: Option<Arc<HashMap<ConnectorId, super::CanonicalQuery>>>,
    ecs: AnswerEcs,
}

/// 一次上游执行的终态和实际选中的配置成员 ID。
struct UpstreamExecutionResult {
    outcome: UpstreamOutcome,
    target_id: Arc<str>,
    used_id: Option<Arc<str>>,
}

impl fmt::Debug for UpstreamRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamRuntime")
            .field("direct_count", &self.direct.len())
            .field("group_count", &self.groups.len())
            .finish()
    }
}

#[derive(Debug)]
struct UpstreamRuntimeBuildError {
    upstream: String,
    reason: String,
}

impl UpstreamRuntime {
    fn from_registry(
        upstreams: &[ResolvedUpstream],
        registry: UpstreamRegistry,
    ) -> Result<Self, UpstreamRuntimeBuildError> {
        let mut definitions = BTreeMap::new();
        for upstream in upstreams {
            let id = upstream_id(upstream).clone();
            if definitions.insert(id.clone(), upstream).is_some() {
                return Err(group_build_error(&id, "duplicate upstream id".to_owned()));
            }
        }

        let mut direct = BTreeMap::new();
        for upstream in upstreams.iter().filter(|upstream| {
            matches!(
                upstream,
                ResolvedUpstream::Hosts { .. } | ResolvedUpstream::Doh { .. }
            )
        }) {
            let id = upstream_id(upstream);
            let exchange = registry
                .get_by_name(id.as_str())
                .map_err(registry_build_error)?;
            if direct.insert(id.clone(), exchange).is_some() {
                return Err(UpstreamRuntimeBuildError {
                    upstream: id.as_str().to_owned(),
                    reason: "duplicate upstream id".to_owned(),
                });
            }
        }

        let mut all = direct.clone();
        let mut groups = BTreeMap::new();
        let mut building = HashSet::new();
        for (id, upstream) in &definitions {
            if matches!(upstream, ResolvedUpstream::Group { .. }) {
                build_group_executor(id, &definitions, &mut all, &mut groups, &mut building)?;
            }
        }

        let explicit_member_ecs = upstreams
            .iter()
            .filter_map(|upstream| match upstream {
                ResolvedUpstream::Doh {
                    id,
                    edns_client_subnet: Some(ecs),
                    ..
                } if ecs.source == ValueSource::Upstream => Some((id.clone(), ecs.clone())),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let group_member_ecs = groups
            .keys()
            .map(|id| {
                let mut members = BTreeMap::new();
                collect_group_member_ecs(
                    id,
                    &definitions,
                    &explicit_member_ecs,
                    &mut HashSet::new(),
                    &mut members,
                );
                (id.clone(), Arc::new(members))
            })
            .collect();

        let target_fingerprints = definitions
            .keys()
            .map(|id| (id.clone(), target_fingerprint(id, &definitions, &registry)))
            .collect();

        Ok(Self {
            direct,
            groups,
            all,
            group_member_ecs,
            target_fingerprints,
        })
    }

    fn len(&self) -> usize {
        self.direct.len() + self.groups.len()
    }

    /// 比较所有可达成员（含继承全局的 fallback）的最终 ECS，保持既有覆盖优先级。
    ///
    /// rule/strategy/client 显式 ECS 统一覆盖成员；成员最终 ECS 一致时退化为单个查询，
    /// 否则保留逐 connector 查询，并以按成员 ID 排序的组合作为答案 key 的 ECS 维度。
    fn prepare_query(
        &self,
        upstream: &ConfigId,
        original_query: &super::CanonicalQuery,
        selected_ecs: &ResolvedEcs,
        client_addr: Option<IpAddr>,
    ) -> PreparedUpstreamQuery {
        let query = effective_upstream_query(original_query, selected_ecs, client_addr);
        let uniform = |query: super::CanonicalQuery| PreparedUpstreamQuery {
            ecs: AnswerEcs::from_query(query.edns_client_subnet()),
            query,
            member_queries: None,
        };
        if !matches!(
            selected_ecs.source,
            ValueSource::Default | ValueSource::Global
        ) {
            return uniform(query);
        }
        let Some(members) = self.group_member_ecs.get(upstream) else {
            return uniform(query);
        };
        if !members.values().any(Option::is_some) {
            return uniform(query);
        }
        let queries: BTreeMap<_, _> = members
            .iter()
            .map(|(member, ecs)| {
                let member_query = ecs.as_ref().map_or_else(
                    || query.clone(),
                    |ecs| effective_upstream_query(original_query, ecs, client_addr),
                );
                (member, member_query)
            })
            .collect();
        // 所有 query 均由同一原始请求只改 ECS 生成，比较规范化后的 ECS 即足够。
        if let Some(first) = queries.values().next()
            && queries
                .values()
                .all(|member| member.edns_client_subnet() == first.edns_client_subnet())
        {
            return uniform(first.clone());
        }
        let ecs = AnswerEcs::members(
            queries
                .iter()
                .map(|(member, query)| (member.as_str(), query.edns_client_subnet())),
        );
        let member_queries = queries
            .into_iter()
            .map(|(member, member_query)| {
                let connector = ConnectorId::new(member.as_str().to_owned())
                    .expect("validated upstream ID must be a connector ID");
                (connector, member_query)
            })
            .collect();
        PreparedUpstreamQuery {
            query,
            member_queries: Some(Arc::new(member_queries)),
            ecs,
        }
    }

    /// 执行 direct upstream 或 group，并把成员级 query 交给 group executor。
    async fn exchange(
        &self,
        upstream: &ConfigId,
        query: &super::CanonicalQuery,
        context: &super::RequestContext,
        late_sink: Option<Arc<dyn LateResultSink>>,
        member_queries: Option<Arc<HashMap<ConnectorId, super::CanonicalQuery>>>,
    ) -> Option<UpstreamExecutionResult> {
        if let Some(exchange) = self.direct.get(upstream) {
            return Some(UpstreamExecutionResult {
                outcome: exchange.exchange(query, context).await,
                target_id: Arc::from(upstream.as_str()),
                used_id: Some(Arc::from(exchange.connector_id().as_str())),
            });
        }
        if let Some(executor) = self.groups.get(upstream) {
            let result = match late_sink {
                Some(sink) => executor
                    .execute_with_selection_and_late_sink(query, context, sink, member_queries)
                    .await
                    .ok(),
                None => executor
                    .execute_with_selection(query, context, member_queries)
                    .await
                    .ok(),
            }?;
            let GroupExecutionResult { connector, outcome } = result;
            return Some(UpstreamExecutionResult {
                target_id: Arc::from(upstream.as_str()),
                used_id: connector.map(|id| Arc::from(id.as_str())),
                outcome,
            });
        }
        if let Some(exchange) = self.all.get(upstream) {
            return Some(UpstreamExecutionResult {
                outcome: exchange.exchange(query, context).await,
                target_id: Arc::from(upstream.as_str()),
                used_id: Some(Arc::from(exchange.connector_id().as_str())),
            });
        }
        None
    }
}

struct GroupExchange {
    connector: ConnectorId,
    executor: Arc<UpstreamGroupExecutor>,
}

impl DnsExchange for GroupExchange {
    fn connector_id(&self) -> &ConnectorId {
        &self.connector
    }

    fn exchange<'a>(
        &'a self,
        query: &'a super::CanonicalQuery,
        context: &'a super::RequestContext,
    ) -> PortFuture<'a, UpstreamOutcome> {
        Box::pin(async move {
            match self.executor.execute(query, context).await {
                Ok(outcome) => outcome,
                Err(_) => UpstreamOutcome::TransportFailure(TransportFailure {
                    connector: self.connector.clone(),
                    class: TransportFailureClass::Internal,
                    retryable: false,
                    safe_context: Some("nested group execution failed"),
                }),
            }
        })
    }
}

fn build_group_executor(
    id: &ConfigId,
    definitions: &BTreeMap<ConfigId, &ResolvedUpstream>,
    all: &mut BTreeMap<ConfigId, Arc<dyn DnsExchange>>,
    groups: &mut BTreeMap<ConfigId, Arc<UpstreamGroupExecutor>>,
    building: &mut HashSet<ConfigId>,
) -> Result<Arc<UpstreamGroupExecutor>, UpstreamRuntimeBuildError> {
    if let Some(executor) = groups.get(id) {
        return Ok(Arc::clone(executor));
    }
    if !building.insert(id.clone()) {
        return Err(group_build_error(id, "nested group cycle".to_owned()));
    }
    let result = (|| {
        let Some(ResolvedUpstream::Group {
            upstreams: members,
            upstream_mode,
            timeout,
            fallbacks,
            fallback_upstream_mode,
            fallback_timeout,
            ..
        }) = definitions.get(id).copied()
        else {
            return Err(group_build_error(
                id,
                "group definition is missing".to_owned(),
            ));
        };
        let selector = GroupSelector::from_upstream_mode(*upstream_mode, members.clone())
            .map_err(|error| group_build_error(id, error.to_string()))?;
        let exchanges =
            group_member_exchanges(definitions, all, groups, building, id, members, "primary")?;
        let executor = if fallbacks.is_empty() {
            UpstreamGroupExecutor::new_with_members(selector, exchanges, *timeout)
        } else {
            let fallback_mode = (*fallback_upstream_mode)
                .ok_or_else(|| group_build_error(id, "fallback mode is missing".to_owned()))?;
            let fallback_timeout = fallback_timeout
                .ok_or_else(|| group_build_error(id, "fallback timeout is missing".to_owned()))?;
            let fallback_selector =
                GroupSelector::from_upstream_mode(fallback_mode, fallbacks.clone())
                    .map_err(|error| group_build_error(id, error.to_string()))?;
            let fallback_exchanges = group_member_exchanges(
                definitions,
                all,
                groups,
                building,
                id,
                fallbacks,
                "fallback",
            )?;
            UpstreamGroupExecutor::new_with_fallback_members(
                selector,
                exchanges,
                *timeout,
                fallback_selector,
                fallback_exchanges,
                fallback_timeout,
            )
        }
        .map_err(|error| group_build_error(id, error.to_string()))?;
        let executor = Arc::new(executor);
        let connector = ConnectorId::new(id.as_str().to_owned())
            .map_err(|_| group_build_error(id, "invalid group connector id".to_owned()))?;
        all.insert(
            id.clone(),
            Arc::new(GroupExchange {
                connector,
                executor: Arc::clone(&executor),
            }),
        );
        groups.insert(id.clone(), Arc::clone(&executor));
        Ok(executor)
    })();
    building.remove(id);
    result
}

fn group_member_exchanges(
    definitions: &BTreeMap<ConfigId, &ResolvedUpstream>,
    all: &mut BTreeMap<ConfigId, Arc<dyn DnsExchange>>,
    groups: &mut BTreeMap<ConfigId, Arc<UpstreamGroupExecutor>>,
    building: &mut HashSet<ConfigId>,
    group: &ConfigId,
    members: &[crate::config::resolve::ResolvedUpstreamMember],
    role: &str,
) -> Result<Vec<GroupMember>, UpstreamRuntimeBuildError> {
    members
        .iter()
        .map(|member| {
            if matches!(
                definitions.get(&member.name),
                Some(ResolvedUpstream::Group { .. })
            ) {
                build_group_executor(&member.name, definitions, all, groups, building)?;
                let executor = groups.get(&member.name).cloned().ok_or_else(|| {
                    group_build_error(group, "nested group connector is missing".to_owned())
                })?;
                let connector =
                    ConnectorId::new(member.name.as_str().to_owned()).map_err(|_| {
                        group_build_error(group, "nested group connector id is invalid".to_owned())
                    })?;
                return Ok(GroupMember::Nested {
                    connector,
                    executor,
                });
            }
            if let Some(exchange) = all.get(&member.name) {
                return Ok(GroupMember::Direct(Arc::clone(exchange)));
            }
            Err(group_build_error(
                group,
                format!(
                    "{role} member `{}` is not a direct connector",
                    member.name.as_str()
                ),
            ))
        })
        .collect()
}

/// 收集 group（含嵌套和 fallback）的全部 direct member；None 表示继承请求级 ECS。
fn collect_group_member_ecs(
    group: &ConfigId,
    definitions: &BTreeMap<ConfigId, &ResolvedUpstream>,
    explicit: &BTreeMap<ConfigId, ResolvedEcs>,
    visited: &mut HashSet<ConfigId>,
    collected: &mut BTreeMap<ConfigId, Option<ResolvedEcs>>,
) {
    if !visited.insert(group.clone()) {
        return;
    }
    let Some(ResolvedUpstream::Group {
        upstreams,
        fallbacks,
        ..
    }) = definitions.get(group).copied()
    else {
        return;
    };
    for member in upstreams.iter().chain(fallbacks) {
        if matches!(
            definitions.get(&member.name).copied(),
            Some(ResolvedUpstream::Group { .. })
        ) {
            collect_group_member_ecs(&member.name, definitions, explicit, visited, collected);
        } else {
            collected.insert(member.name.clone(), explicit.get(&member.name).cloned());
        }
    }
}

/// 计算 target 子树的内容摘要，作为答案 key 的 target 维度。
///
/// 覆盖 group 的成员、权重、fallback、模式、超时与嵌套结构，以及 direct 上游的已解析定义、
/// 完整 DoH URL、`connect_ip`、bootstrap 链与所用 outbound。成员 ECS 已体现在答案 key 的
/// ECS 维度中，不重复纳入；与该子树无关的配置变化不会改变摘要。
fn target_fingerprint(
    id: &ConfigId,
    definitions: &BTreeMap<ConfigId, &ResolvedUpstream>,
    registry: &UpstreamRegistry,
) -> CacheFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(b"fluxdns/cache-target/v3\0");
    update_target_fingerprint(&mut hasher, id, definitions, registry);
    CacheFingerprint::from_digest(hasher.finalize().into())
}

/// 配置校验已拒绝 group 与 bootstrap 环，递归必然终止。
fn update_target_fingerprint(
    hasher: &mut Sha256,
    id: &ConfigId,
    definitions: &BTreeMap<ConfigId, &ResolvedUpstream>,
    registry: &UpstreamRegistry,
) {
    update_fingerprint_component(hasher, id.as_str().as_bytes());
    match definitions.get(id).copied() {
        Some(ResolvedUpstream::Hosts { format, hosts, .. }) => {
            update_fingerprint_component(hasher, b"hosts");
            update_fingerprint_component(hasher, format.as_bytes());
            update_fingerprint_component(hasher, hosts.as_bytes());
        }
        Some(ResolvedUpstream::Doh {
            address,
            bootstrap,
            connect_ip,
            proxy,
            ..
        }) => {
            // SafeUrl 的 Debug 会隐藏路径和查询参数，完整 URL 只进入摘要。
            update_fingerprint_component(hasher, b"doh");
            update_fingerprint_component(hasher, address.as_str().as_bytes());
            update_fingerprint_component(hasher, format!("{connect_ip:?}").as_bytes());
            match bootstrap {
                Some(bootstrap) => {
                    update_fingerprint_component(hasher, b"bootstrap");
                    update_target_fingerprint(hasher, bootstrap, definitions, registry);
                }
                None => update_fingerprint_component(hasher, b""),
            }
            match proxy
                .as_ref()
                .and_then(|id| registry.outbound_fingerprint(id))
            {
                Some(digest) => update_fingerprint_component(hasher, digest),
                None => update_fingerprint_component(hasher, b""),
            }
        }
        Some(ResolvedUpstream::Group {
            upstreams,
            upstream_mode,
            timeout,
            fallbacks,
            fallback_upstream_mode,
            fallback_timeout,
            ..
        }) => {
            update_fingerprint_component(hasher, b"group");
            update_fingerprint_component(
                hasher,
                format!(
                    "{upstream_mode:?}|{timeout:?}|{fallback_upstream_mode:?}|{fallback_timeout:?}"
                )
                .as_bytes(),
            );
            for members in [upstreams, fallbacks] {
                update_fingerprint_component(hasher, &(members.len() as u64).to_be_bytes());
                for member in members {
                    update_fingerprint_component(hasher, &member.weight.to_be_bytes());
                    update_target_fingerprint(hasher, &member.name, definitions, registry);
                }
            }
        }
        None => {}
    }
}

fn group_build_error(group: &ConfigId, reason: String) -> UpstreamRuntimeBuildError {
    UpstreamRuntimeBuildError {
        upstream: group.as_str().to_owned(),
        reason,
    }
}

fn direct_upstreams(upstreams: &[ResolvedUpstream]) -> Vec<ResolvedUpstream> {
    upstreams
        .iter()
        .filter(|upstream| {
            matches!(
                upstream,
                ResolvedUpstream::Hosts { .. } | ResolvedUpstream::Doh { .. }
            )
        })
        .cloned()
        .collect()
}

fn upstream_id(upstream: &ResolvedUpstream) -> &ConfigId {
    match upstream {
        ResolvedUpstream::Hosts { id, .. }
        | ResolvedUpstream::Doh { id, .. }
        | ResolvedUpstream::Group { id, .. } => id,
    }
}

fn registry_build_error(error: RegistryError) -> UpstreamRuntimeBuildError {
    let upstream = match &error {
        RegistryError::InvalidConnectorId { upstream }
        | RegistryError::InvalidHosts { upstream }
        | RegistryError::InvalidDoh { upstream }
        | RegistryError::InvalidDohTransport { upstream }
        | RegistryError::UnsupportedUpstream { upstream, .. } => upstream.clone(),
        RegistryError::InvalidOutbound { outbound, .. }
        | RegistryError::DuplicateOutbound { outbound } => outbound.clone(),
        RegistryError::MissingOutbound { upstream, .. }
        | RegistryError::InvalidOutboundCombination { upstream, .. } => upstream.clone(),
        RegistryError::DuplicateConnector { connector }
        | RegistryError::MissingConnector { connector } => connector.clone(),
    };
    UpstreamRuntimeBuildError {
        upstream,
        reason: error.to_string(),
    }
}

fn servfail(request: &DnsRequest) -> Result<CoreOutcome, CoreError> {
    CanonicalResponse::empty_response(&request.query, ResponseCode::ServFail)
        .map(Arc::new)
        .map(CoreOutcome::Response)
        .map_err(CoreError::ResponseConstruction)
}

#[cfg(test)]
#[path = "policy_cache_tests.rs"]
mod cache_regression_tests;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{
        Name, RData, Record, RecordType,
        rdata::{
            A,
            opt::{EdnsCode, EdnsOption},
        },
    };
    use ipnet::IpNet;

    use crate::cache::{CacheLookup, CacheSnapshotOwner, CacheSnapshotSettings};
    use crate::config::model::{EcsMode, RuleSetFormat};
    use crate::config::resolve::{
        ConfigId, ResolvedCacheOverride, ResolvedClient, ResolvedEcs, ResolvedOutbound,
        ResolvedRuleSet, ResolvedRuleSetRef, ResolvedSecretRef, ResolvedStrategyRule,
        ResolvedTtlOverride, ResolvedUpstream, ResolvedUpstreamMember, ValueSource,
    };
    use crate::config::{ConfigV2Loader, LoadOptions};
    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, CanonicalResponse, ClientId,
        CoreOutcome, Deadline, DnsCore, DnsRequest, ListenerId, MatchedRuleSource, RequestContext,
        RequestId, RequestMeta, RuntimeRevision, TransportCapabilities, TransportClass,
    };
    use crate::ports::cache::CacheUpstreamId;
    use crate::ports::exchange::{ConnectorId, UpstreamOutcome};
    use crate::ports::{PortError, PortFuture};
    use crate::resource::{
        CanonicalDomain, ResourceSnapshot, ResourceSourceKind, ResourceStaleStatus,
        ResourceVersion, RuleIndex,
    };
    use crate::upstream::{
        DohHttpRequest, DohHttpResponseOwned, DohHttpTransport, UpstreamAttempt,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{
        PolicyDnsCore, RuntimeCoreCell, RuntimeCoreTarget, UpstreamRuntime, answer_key, cache_key,
        cache_upstream_provenance, effective_upstream_query, stale_answer_ttl,
    };
    use crate::upstream::UpstreamRegistry;

    struct FakeDohTransport {
        request: Mutex<Option<DohHttpRequest>>,
        calls: AtomicUsize,
        positive: bool,
    }

    struct ServFailDohTransport;

    struct GatedPositiveDohTransport {
        gate: Arc<crate::ports::testing::TestGate>,
    }

    impl DohHttpTransport for GatedPositiveDohTransport {
        fn post<'a>(
            &'a self,
            request: DohHttpRequest,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
            Box::pin(async move {
                self.gate.pause().await;
                let message = Message::from_vec(request.body()).unwrap();
                let query = CanonicalQuery::from_message(message.clone()).unwrap();
                let mut response = CanonicalResponse::response_with_answers(
                    &query,
                    [Record::from_rdata(
                        query.question().name().clone(),
                        30,
                        RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, 20))),
                    )],
                )
                .unwrap()
                .as_message()
                .clone();
                response.metadata.id = message.metadata.id;
                Ok(DohHttpResponseOwned {
                    status: 200,
                    content_type: Some("application/dns-message".into()),
                    body: response.to_vec().unwrap(),
                })
            })
        }
    }

    impl DohHttpTransport for ServFailDohTransport {
        fn post<'a>(
            &'a self,
            request: DohHttpRequest,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
            Box::pin(async move {
                let query = Message::from_vec(request.body()).unwrap();
                let mut response =
                    Message::new(query.metadata.id, MessageType::Response, OpCode::Query);
                response.metadata.response_code = ResponseCode::ServFail;
                response.add_query(query.queries[0].clone());
                Ok(DohHttpResponseOwned {
                    status: 200,
                    content_type: Some("application/dns-message".to_owned()),
                    body: response.to_vec().unwrap(),
                })
            })
        }
    }

    impl FakeDohTransport {
        fn new() -> Self {
            Self {
                request: Mutex::new(None),
                calls: AtomicUsize::new(0),
                positive: false,
            }
        }

        fn positive() -> Self {
            Self {
                positive: true,
                ..Self::new()
            }
        }
    }

    impl DohHttpTransport for FakeDohTransport {
        fn post<'a>(
            &'a self,
            request: DohHttpRequest,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let request_body = request.body().to_vec();
            *self.request.lock().unwrap() = Some(request);
            let query = Message::from_vec(&request_body).unwrap();
            let mut response =
                Message::new(query.metadata.id, MessageType::Response, OpCode::Query);
            response.metadata.response_code = ResponseCode::NoError;
            response.add_query(query.queries[0].clone());
            if self.positive {
                response.add_answer(Record::from_rdata(
                    query.queries[0].name().clone(),
                    30,
                    RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, 30))),
                ));
            }
            let body = response.to_vec().unwrap();
            Box::pin(async move {
                Ok(DohHttpResponseOwned {
                    status: 200,
                    content_type: Some("application/dns-message".to_owned()),
                    body,
                })
            })
        }
    }

    #[test]
    fn upstream_runtime_registers_plain_http_doh_through_registry() {
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("remote").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: Some("192.0.2.44".parse().unwrap()),
            proxy: None,
            edns_client_subnet: Some(ResolvedEcs {
                mode: EcsMode::Disabled,
                custom_ip: None,
                source: ValueSource::Upstream,
            }),
        };

        let registry = UpstreamRegistry::from_resolved(std::slice::from_ref(&doh)).unwrap();
        let runtime = UpstreamRuntime::from_registry(&[doh], registry).unwrap();
        let connector = runtime
            .direct
            .get(&ConfigId::new("remote").unwrap())
            .expect("plain HTTP DoH connector must be registered");
        assert_eq!(connector.connector_id().as_str(), "remote");
        assert_eq!(runtime.len(), 1);
    }

    #[test]
    fn upstream_runtime_accepts_direct_https_doh() {
        let core = PolicyDnsCore::from_config(
            doh_config_with_address("https://dns.example.test/dns-query").as_ref(),
            42,
        )
        .unwrap();
        assert_eq!(core.upstream_count(), 1);
    }

    #[test]
    fn cache_facade_follows_global_cache_configuration() {
        let disabled = PolicyDnsCore::from_config(doh_config().as_ref(), 42).unwrap();
        assert!(!disabled.cache().options().enabled);
        assert!(!disabled.cache().options().optimistic_enabled);

        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        config.dns.cache.optimistic.enabled = true;
        let enabled = PolicyDnsCore::from_config(&config, 42).unwrap();
        assert!(enabled.cache().options().enabled);
        assert!(enabled.cache().options().optimistic_enabled);
    }

    #[tokio::test]
    async fn strategy_cache_works_when_global_pool_is_disabled() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        assert!(!config.dns.cache.enabled);
        config.strategies[0].cache = Some(ResolvedCacheOverride {
            enabled: true,
            optimistic: Some(config.dns.cache.optimistic.clone()),
            source: ValueSource::Strategy,
        });
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();

        core.resolve(&request("strategy-cache.example.", RecordType::A))
            .await
            .unwrap();
        core.resolve(&request("strategy-cache.example.", RecordType::A))
            .await
            .unwrap();

        assert!(core.cache().options().enabled);
        assert_eq!(transport.calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn client_cache_uses_matched_identity_and_isolates_clients() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        assert!(!config.dns.cache.enabled);
        config.clients.push(ResolvedClient {
            name: ConfigId::new("authenticated").unwrap(),
            client_ids: vec!["alice".to_owned(), "bob".to_owned()],
            ips: Vec::new(),
            strategy: None,
            cache: Some(ResolvedCacheOverride {
                enabled: true,
                optimistic: Some(config.dns.cache.optimistic.clone()),
                source: ValueSource::Client,
            }),
            ttl_override: ResolvedTtlOverride {
                enabled: false,
                min: None,
                max: None,
                source: ValueSource::Default,
            },
            edns_client_subnet: ResolvedEcs {
                mode: EcsMode::Disabled,
                custom_ip: None,
                source: ValueSource::Default,
            },
        });
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();
        let mut alice = request("client-cache.example.", RecordType::A);
        alice.context.client.client_id = Some(ClientId::from("alice"));
        let mut bob = request("client-cache.example.", RecordType::A);
        bob.context.client.client_id = Some(ClientId::from("bob"));

        core.resolve(&alice).await.unwrap();
        core.resolve(&alice).await.unwrap();
        core.resolve(&bob).await.unwrap();
        core.resolve(&bob).await.unwrap();

        assert!(core.cache().options().enabled);
        assert_eq!(transport.calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn cache_snapshot_recovers_across_policy_core_instances() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns")
            .join("p2-cache-owner-policy-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        config.dns.cache.persistence_path = root.join("cache.snapshot");
        let deadline = || Deadline::new(Instant::now() + Duration::from_secs(5));

        let first_transport = Arc::new(FakeDohTransport::new());
        let first_registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            first_transport.clone(),
        )
        .unwrap();
        let first = PolicyDnsCore::from_config_with_registry(&config, 42, first_registry).unwrap();
        let first_owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            first.cache_snapshot_source(),
            CacheSnapshotSettings::new(
                true,
                config.dns.cache.persistence_path.clone(),
                Duration::from_secs(3600),
                Vec::new(),
            )
            .unwrap(),
            deadline(),
        )
        .await
        .unwrap();
        assert_eq!(first_owner.status().recovery.loaded, 0);
        first
            .resolve(&request("persistent-cache.example.", RecordType::A))
            .await
            .unwrap();
        assert_eq!(first_transport.calls.load(Ordering::Acquire), 1);
        let first_shutdown = first.finalizer_owner().shutdown_until(deadline()).await;
        assert!(first_shutdown.completed, "shutdown: {first_shutdown:?}");
        let snapshot_shutdown = first_owner.shutdown(deadline()).await;
        assert!(
            snapshot_shutdown.completed,
            "shutdown: {snapshot_shutdown:?}"
        );
        assert!(snapshot_shutdown.written, "shutdown: {snapshot_shutdown:?}");
        assert_eq!(
            &std::fs::read(&config.dns.cache.persistence_path).unwrap()[..4],
            b"FDCS"
        );
        assert!(
            !config
                .dns
                .cache
                .persistence_path
                .with_extension("snapshot-wal")
                .exists()
        );
        assert!(
            !config
                .dns
                .cache
                .persistence_path
                .with_extension("snapshot-shm")
                .exists()
        );

        let second_transport = Arc::new(FakeDohTransport::new());
        let second_registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            second_transport.clone(),
        )
        .unwrap();
        let second =
            PolicyDnsCore::from_config_with_registry(&config, 42, second_registry).unwrap();
        let second_owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            second.cache_snapshot_source(),
            CacheSnapshotSettings::new(
                true,
                config.dns.cache.persistence_path.clone(),
                Duration::from_secs(3600),
                Vec::new(),
            )
            .unwrap(),
            deadline(),
        )
        .await
        .unwrap();
        assert_eq!(
            second_owner.status().recovery.loaded,
            1,
            "unexpected recovery status: {:?}",
            second_owner.status()
        );
        second
            .resolve(&request("persistent-cache.example.", RecordType::A))
            .await
            .unwrap();
        assert_eq!(second_transport.calls.load(Ordering::Acquire), 0);
        assert!(
            second
                .finalizer_owner()
                .shutdown_until(deadline())
                .await
                .completed
        );
        assert!(second_owner.shutdown(deadline()).await.completed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn policy_core_uses_injected_registry_for_doh_exchange() {
        let config = doh_config();
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();

        let outcome = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(response) = outcome else {
            panic!("expected injected DoH response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::NoData);

        let request = transport.request.lock().unwrap().clone().unwrap();
        assert_eq!(
            request.endpoint().as_str(),
            "http://dns.example.test/dns-query"
        );
        assert_eq!(request.connect_ip(), Some("192.0.2.44".parse().unwrap()));
        assert_eq!(Message::from_vec(request.body()).unwrap().id, 1);
    }

    #[tokio::test]
    async fn policy_core_applies_global_custom_ecs_to_upstream_query() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        let ecs = ResolvedEcs {
            mode: EcsMode::Custom,
            custom_ip: Some("203.0.113.0/24".parse().unwrap()),
            source: ValueSource::Global,
        };
        config.dns.edns_client_subnet = ecs.clone();
        config.strategies[0].edns_client_subnet = ecs.clone();
        let ResolvedUpstream::Doh {
            edns_client_subnet, ..
        } = &mut config.upstreams[0]
        else {
            panic!("fixture must contain a DoH upstream");
        };
        *edns_client_subnet = Some(ecs);
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();

        core.resolve(&request("ecs.example.", RecordType::A))
            .await
            .unwrap();

        let guard = transport.request.lock().unwrap();
        let wire = Message::from_vec(guard.as_ref().unwrap().body()).unwrap();
        let option = wire
            .edns
            .as_ref()
            .and_then(|edns| edns.option(EdnsCode::Subnet));
        assert!(matches!(
            option,
            Some(EdnsOption::Subnet(subnet))
                if subnet.addr() == IpAddr::from([203, 0, 113, 0])
                    && subnet.source_prefix() == 24
        ));
    }

    #[tokio::test]
    async fn policy_core_applies_explicit_upstream_custom_ecs_to_query() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        let ResolvedUpstream::Doh {
            edns_client_subnet, ..
        } = &mut config.upstreams[0]
        else {
            panic!("fixture must contain a DoH upstream");
        };
        *edns_client_subnet = Some(ResolvedEcs {
            mode: EcsMode::Custom,
            custom_ip: Some("198.51.100.0/24".parse().unwrap()),
            source: ValueSource::Upstream,
        });
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();

        core.resolve(&request("upstream-ecs.example.", RecordType::A))
            .await
            .unwrap();

        let guard = transport.request.lock().unwrap();
        let wire = Message::from_vec(guard.as_ref().unwrap().body()).unwrap();
        let option = wire
            .edns
            .as_ref()
            .and_then(|edns| edns.option(EdnsCode::Subnet));
        assert!(matches!(
            option,
            Some(EdnsOption::Subnet(subnet))
                if subnet.addr() == IpAddr::from([198, 51, 100, 0])
                    && subnet.source_prefix() == 24
        ));
    }

    #[tokio::test]
    async fn group_member_ecs_overrides_global_ecs_and_caches_uniform_group() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        let global_ecs = ResolvedEcs {
            mode: EcsMode::Custom,
            custom_ip: Some("203.0.113.0/24".parse().unwrap()),
            source: ValueSource::Global,
        };
        config.dns.edns_client_subnet = global_ecs.clone();
        config.strategies[0].edns_client_subnet = global_ecs;
        config.dns.cache.enabled = true;
        let ResolvedUpstream::Doh {
            edns_client_subnet, ..
        } = &mut config.upstreams[0]
        else {
            panic!("fixture must contain a DoH upstream");
        };
        *edns_client_subnet = Some(ResolvedEcs {
            mode: EcsMode::Custom,
            custom_ip: Some("198.51.100.0/24".parse().unwrap()),
            source: ValueSource::Upstream,
        });
        route_doh_through_nested_single_member_group(&mut config);
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &super::direct_upstreams(&config.upstreams),
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();

        let (_, observation) = core
            .resolve_with_observation(&request("group-member-ecs.example.", RecordType::A))
            .await;
        core.resolve(&request("group-member-ecs.example.", RecordType::A))
            .await
            .unwrap();

        assert_eq!(transport.calls.load(Ordering::Acquire), 1);
        let observation = observation.expect("group member ECS must report metadata");
        assert_eq!(observation.upstream_id.as_deref(), Some("group"));
        assert_eq!(observation.upstream_member_id.as_deref(), Some("inner"));
        let guard = transport.request.lock().unwrap();
        let wire = Message::from_vec(guard.as_ref().unwrap().body()).unwrap();
        assert!(matches!(
            wire.edns
                .as_ref()
                .and_then(|edns| edns.option(EdnsCode::Subnet)),
            Some(EdnsOption::Subnet(subnet))
                if subnet.addr() == IpAddr::from([198, 51, 100, 0])
                    && subnet.source_prefix() == 24
        ));
    }

    #[tokio::test]
    async fn strategy_ecs_overrides_group_member_ecs() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.strategies[0].edns_client_subnet = ResolvedEcs {
            mode: EcsMode::Custom,
            custom_ip: Some("203.0.113.0/24".parse().unwrap()),
            source: ValueSource::Strategy,
        };
        let ResolvedUpstream::Doh {
            edns_client_subnet, ..
        } = &mut config.upstreams[0]
        else {
            panic!("fixture must contain a DoH upstream");
        };
        *edns_client_subnet = Some(ResolvedEcs {
            mode: EcsMode::Custom,
            custom_ip: Some("198.51.100.0/24".parse().unwrap()),
            source: ValueSource::Upstream,
        });
        route_doh_through_nested_single_member_group(&mut config);
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &super::direct_upstreams(&config.upstreams),
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();

        core.resolve(&request("strategy-group-ecs.example.", RecordType::A))
            .await
            .unwrap();

        let guard = transport.request.lock().unwrap();
        let wire = Message::from_vec(guard.as_ref().unwrap().body()).unwrap();
        assert!(matches!(
            wire.edns
                .as_ref()
                .and_then(|edns| edns.option(EdnsCode::Subnet)),
            Some(EdnsOption::Subnet(subnet))
                if subnet.addr() == IpAddr::from([203, 0, 113, 0])
                    && subnet.source_prefix() == 24
        ));
    }

    #[test]
    fn client_mode_prefers_and_normalizes_request_ecs() {
        let supplied = hickory_proto::rr::rdata::opt::ClientSubnet::new(
            IpAddr::from([198, 51, 100, 42]),
            24,
            0,
        );
        let query = request("ecs-client.example.", RecordType::A)
            .query
            .with_edns_client_subnet(Some(supplied));
        let ecs = ResolvedEcs {
            mode: EcsMode::Client,
            custom_ip: None,
            source: ValueSource::Strategy,
        };

        let query = effective_upstream_query(&query, &ecs, Some(IpAddr::from([203, 0, 113, 10])));

        let subnet = query.edns_client_subnet().unwrap();
        assert_eq!(subnet.addr(), IpAddr::from([198, 51, 100, 0]));
        assert_eq!(subnet.source_prefix(), 24);
    }

    /// 验证非法客户端 ECS 不会进入上游或 cache key，并安全回退到脱敏客户端网段。
    #[test]
    fn client_mode_rejects_invalid_request_ecs_and_uses_safe_fallback() {
        let invalid = hickory_proto::rr::rdata::opt::ClientSubnet::new(
            IpAddr::from([198, 51, 100, 42]),
            33,
            0,
        );
        let query = request("invalid-ecs-client.example.", RecordType::A)
            .query
            .with_edns_client_subnet(Some(invalid));
        let ecs = ResolvedEcs {
            mode: EcsMode::Client,
            custom_ip: None,
            source: ValueSource::Strategy,
        };

        let fallback =
            effective_upstream_query(&query, &ecs, Some(IpAddr::from([203, 0, 113, 10])));
        let subnet = fallback.edns_client_subnet().unwrap();
        assert_eq!(subnet.addr(), IpAddr::from([203, 0, 113, 0]));
        assert_eq!(subnet.source_prefix(), 24);

        let without_client = effective_upstream_query(&query, &ecs, None);
        assert!(without_client.edns_client_subnet().is_none());
    }

    #[tokio::test]
    async fn client_ecs_subnets_isolate_cache_entries() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        config.strategies[0].edns_client_subnet = ResolvedEcs {
            mode: EcsMode::Client,
            custom_ip: None,
            source: ValueSource::Strategy,
        };
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();
        let first = request("ecs-cache.example.", RecordType::A);
        let mut second = first.clone();
        second.context.client.client_addr = Some(IpAddr::from([192, 0, 2, 2]));
        let mut same_subnet = first.clone();
        same_subnet.context.client.client_addr = Some(IpAddr::from([192, 0, 2, 99]));

        core.resolve(&first).await.unwrap();
        core.resolve(&second).await.unwrap();
        core.resolve(&same_subnet).await.unwrap();

        assert_eq!(transport.calls.load(Ordering::Acquire), 2);
        let guard = transport.request.lock().unwrap();
        let wire = Message::from_vec(guard.as_ref().unwrap().body()).unwrap();
        assert!(matches!(
            wire.edns
                .as_ref()
                .and_then(|edns| edns.option(EdnsCode::Subnet)),
            Some(EdnsOption::Subnet(subnet))
                if subnet.addr() == IpAddr::from([192, 0, 2, 0])
                    && subnet.source_prefix() == 24
        ));
    }

    #[tokio::test]
    async fn policy_core_observation_reports_strategy_source_and_cache_status() {
        let config = doh_config();
        let transport = Arc::new(FakeDohTransport::new());
        let registry =
            UpstreamRegistry::from_resolved_with_doh_transport(&config.upstreams, transport)
                .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();

        let (_, observation) = core
            .resolve_with_observation(&request("remote.example.", RecordType::A))
            .await;
        let observation = observation.expect("policy core must report metadata");
        assert_eq!(observation.strategy_id.as_deref(), Some("default"));
        assert_eq!(observation.upstream_id.as_deref(), Some("remote"));
        assert_eq!(observation.upstream_member_id, None);
        assert_eq!(observation.upstream_used_id.as_deref(), Some("remote"));
        assert_eq!(
            observation.source,
            crate::ports::storage::StatsSource::Upstream
        );
        assert_eq!(
            observation.cache_status,
            crate::ports::telemetry::CacheStatus::Disabled
        );

        let mut cached_config = Arc::try_unwrap(doh_config()).unwrap();
        cached_config.dns.cache.enabled = true;
        let cached_config = Arc::new(cached_config);
        let transport = Arc::new(FakeDohTransport::new());
        let registry =
            UpstreamRegistry::from_resolved_with_doh_transport(&cached_config.upstreams, transport)
                .unwrap();
        let core =
            PolicyDnsCore::from_config_with_registry(cached_config.as_ref(), 42, registry).unwrap();
        let (result, _) = core
            .resolve_with_observation(&request("remote.example.", RecordType::A))
            .await;
        assert!(result.is_ok());
        let (_, observation) = core
            .resolve_with_observation(&request("remote.example.", RecordType::A))
            .await;
        let observation = observation.expect("cached policy core must report metadata");
        assert_eq!(observation.upstream_id.as_deref(), Some("remote"));
        assert_eq!(observation.upstream_member_id, None);
        assert_eq!(observation.upstream_used_id.as_deref(), Some("remote"));
        assert_eq!(
            observation.source,
            crate::ports::storage::StatsSource::Cache
        );
        assert_eq!(
            observation.cache_status,
            crate::ports::telemetry::CacheStatus::Fresh
        );
    }

    #[tokio::test]
    async fn policy_core_observation_freezes_matched_client_identity() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.clients.push(ResolvedClient {
            name: ConfigId::new("office").unwrap(),
            client_ids: vec!["Office-01".to_owned()],
            ips: vec![IpNet::from_str("127.0.0.0/8").unwrap()],
            strategy: None,
            cache: None,
            ttl_override: ResolvedTtlOverride {
                enabled: false,
                min: None,
                max: None,
                source: ValueSource::Default,
            },
            edns_client_subnet: ResolvedEcs {
                mode: EcsMode::Disabled,
                custom_ip: None,
                source: ValueSource::Default,
            },
        });
        let config = Arc::new(config);
        let transport = Arc::new(FakeDohTransport::new());
        let registry =
            UpstreamRegistry::from_resolved_with_doh_transport(&config.upstreams, transport)
                .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();

        let (_, observation) = core
            .resolve_with_observation(&request("remote.example.", RecordType::A))
            .await;
        let observation =
            observation.expect("matched client must be included in policy observation");
        assert_eq!(observation.client_bucket.as_deref(), Some("office"));
        let matched = observation
            .client_match
            .expect("IP match must freeze the stable client ID");
        assert_eq!(
            matched.source,
            crate::ports::observation::ClientMatchSource::Ip
        );
        assert_eq!(matched.matched_client_id.as_ref(), "Office-01");
    }

    #[tokio::test]
    async fn policy_core_caches_upstream_response_and_coalesces_lookup() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        let config = Arc::new(config);
        let transport = Arc::new(FakeDohTransport::new());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();

        let first = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap();
        let second = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap();
        assert!(
            matches!(first, CoreOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NoData)
        );
        assert!(
            matches!(second, CoreOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NoData)
        );
        assert_eq!(transport.calls.load(Ordering::Acquire), 1);
        assert_eq!(core.cache().store().stats().hits, 1);
    }

    #[tokio::test]
    async fn cache_hits_apply_remaining_and_stale_answer_ttl() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        config.dns.cache.optimistic.enabled = true;
        config.dns.cache.optimistic.answer_ttl = Duration::from_secs(7);
        config.dns.cache.optimistic.max_age = Duration::from_secs(60);
        config.dns.cache.optimistic.negative_max_age = Duration::from_secs(20);
        let config = Arc::new(config);
        let transport = Arc::new(FakeDohTransport::new());
        let registry =
            UpstreamRegistry::from_resolved_with_doh_transport(&config.upstreams, transport)
                .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();
        let request = request("ttl-cache.example.", RecordType::A);
        let qname = CanonicalDomain::parse(&request.query.question().name().to_ascii()).unwrap();
        let listener_id = ConfigId::new("dns").unwrap();
        let plan = core
            .policy()
            .evaluate(crate::policy::PolicyRequest {
                listener_id: &listener_id,
                doh_route_id: None,
                client_id: None,
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&core, &plan, &request).unwrap();
        let response = Arc::new(
            CanonicalResponse::response_with_answers(
                &request.query,
                [Record::from_rdata(
                    request.query.question().name().clone(),
                    120,
                    RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, 20))),
                )],
            )
            .unwrap(),
        );
        // 乐观窗口按条目质量选择：完整应答 60s、负应答 20s、失败类不乐观返回。
        let max_age_now = Instant::now();
        let entry_with = |quality, class, expired_for: u64| crate::ports::cache::CacheEntry {
            response: Arc::clone(&response),
            upstream: cache_upstream_provenance("remote", Some("remote")),
            inserted_at: max_age_now - Duration::from_secs(expired_for + 10),
            expires_at: max_age_now - Duration::from_secs(expired_for),
            // 模拟快照中按旧规则写入的长窗口，验证 lookup 侧按当前池收紧。
            stale_until: Some(max_age_now + Duration::from_secs(86_400)),
            response_class: class,
            producer_revision: RuntimeRevision(1),
            quality,
            checksum: 1,
            format_version: crate::ports::cache::CACHE_ENTRY_FORMAT_VERSION,
        };
        use crate::ports::cache::{CacheQuality as Q, CacheResponseClass as C};
        let cases = [
            (Q::Complete, C::NoError, 59, Some(Duration::from_secs(7))),
            (Q::Complete, C::NoError, 61, None),
            (Q::Negative, C::NoData, 19, Some(Duration::from_secs(7))),
            (Q::Negative, C::NoData, 21, None),
            (Q::Negative, C::NxDomain, 21, None),
            (Q::Failure, C::ServFail, 1, None),
        ];
        for (quality, class, expired_for, expected) in cases {
            assert_eq!(
                stale_answer_ttl(
                    &plan.cache,
                    &entry_with(quality, class, expired_for),
                    max_age_now
                ),
                expected,
                "quality={quality:?} class={class:?} expired_for={expired_for}"
            );
        }
        let now = Instant::now();
        let fresh_entry = Arc::new(crate::ports::cache::CacheEntry {
            response: Arc::clone(&response),
            upstream: cache_upstream_provenance("remote", Some("remote")),
            inserted_at: now - Duration::from_secs(20),
            expires_at: now + Duration::from_secs(100),
            stale_until: Some(now + Duration::from_secs(160)),
            response_class: crate::ports::cache::CacheResponseClass::NoError,
            producer_revision: RuntimeRevision(1),
            quality: crate::ports::cache::CacheQuality::Complete,
            checksum: 1,
            format_version: crate::ports::cache::CACHE_ENTRY_FORMAT_VERSION,
        });
        let version = match core
            .cache()
            .store()
            .compare_and_swap(
                key.clone(),
                crate::ports::cache::CacheCondition::Absent,
                fresh_entry,
                Deadline::new(Instant::now() + Duration::from_secs(1)),
            )
            .await
            .unwrap()
        {
            crate::ports::cache::CacheWriteOutcome::Inserted(version) => version,
            other => panic!("expected inserted cache entry, got {other:?}"),
        };

        let CoreOutcome::Response(fresh) = core.resolve(&request).await.unwrap() else {
            panic!("expected fresh cache response");
        };
        assert!((99..=100).contains(&fresh.as_message().answers[0].ttl));

        let stale_now = Instant::now();
        let stale_entry = Arc::new(crate::ports::cache::CacheEntry {
            response,
            upstream: cache_upstream_provenance("remote", Some("remote")),
            inserted_at: stale_now - Duration::from_secs(121),
            expires_at: stale_now - Duration::from_secs(1),
            stale_until: Some(stale_now + Duration::from_secs(60)),
            response_class: crate::ports::cache::CacheResponseClass::NoError,
            producer_revision: RuntimeRevision(1),
            quality: crate::ports::cache::CacheQuality::Complete,
            checksum: 1,
            format_version: crate::ports::cache::CACHE_ENTRY_FORMAT_VERSION,
        });
        core.cache()
            .store()
            .compare_and_swap(
                key,
                crate::ports::cache::CacheCondition::Version(version),
                stale_entry,
                Deadline::new(Instant::now() + Duration::from_secs(1)),
            )
            .await
            .unwrap();

        let CoreOutcome::Response(stale) = core.resolve(&request).await.unwrap() else {
            panic!("expected stale cache response");
        };
        assert_eq!(stale.as_message().answers[0].ttl, 7);
    }

    #[tokio::test]
    async fn optimistic_stale_lookup_refreshes_through_late_finalizer() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        config.dns.cache.optimistic.enabled = true;
        let config = Arc::new(config);
        let transport = Arc::new(FakeDohTransport::positive());
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            transport.clone(),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();

        let first_request = request("remote.example.", RecordType::A);
        let qname =
            CanonicalDomain::parse(&first_request.query.question().name().to_ascii()).unwrap();
        let listener_id = ConfigId::new("dns").unwrap();
        let plan = core
            .policy()
            .evaluate(crate::policy::PolicyRequest {
                listener_id: &listener_id,
                doh_route_id: None,
                client_id: None,
                client_addr: first_request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&core, &plan, &first_request).unwrap();
        let first = core.resolve(&first_request).await.unwrap();
        assert!(
            matches!(first, CoreOutcome::Response(response) if response.class() == crate::dns::ResponseClass::Positive)
        );

        let record = match core
            .cache()
            .lookup(&key, Deadline::new(Instant::now() + Duration::from_secs(1)))
            .await
            .unwrap()
        {
            CacheLookup::Fresh(record) => record,
            other => panic!("expected a fresh cache record, got {other:?}"),
        };
        let now = Instant::now();
        let stale_entry = crate::ports::cache::CacheEntry {
            response: Arc::clone(&record.entry.response),
            upstream: record.entry.upstream.clone(),
            inserted_at: now - Duration::from_secs(1),
            expires_at: now - Duration::from_millis(1),
            stale_until: Some(now + Duration::from_secs(5)),
            response_class: record.entry.response_class,
            producer_revision: record.entry.producer_revision,
            quality: record.entry.quality,
            checksum: record.entry.checksum,
            format_version: record.entry.format_version,
        };
        core.cache()
            .store()
            .compare_and_swap(
                key.clone(),
                crate::ports::cache::CacheCondition::Version(record.version),
                Arc::new(stale_entry),
                Deadline::new(Instant::now() + Duration::from_secs(1)),
            )
            .await
            .unwrap();

        let stale_request = request("remote.example.", RecordType::A);
        let stale = core.resolve(&stale_request).await.unwrap();
        assert!(
            matches!(stale, CoreOutcome::Response(response) if response.class() == crate::dns::ResponseClass::Positive)
        );

        let mut refreshed = false;
        for _ in 0..100 {
            if transport.calls.load(Ordering::Acquire) >= 2
                && matches!(
                    core.cache()
                        .lookup(&key, Deadline::new(Instant::now() + Duration::from_secs(1)))
                        .await
                        .unwrap(),
                    CacheLookup::Fresh(_)
                )
            {
                refreshed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(refreshed, "stale lookup must complete a bounded refresh");
        assert_eq!(transport.calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn optimistic_refresh_targets_the_latest_runtime_snapshot() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        config.dns.cache.optimistic.enabled = true;
        let config = Arc::new(config);
        let old_transport = Arc::new(FakeDohTransport::positive());
        let latest_transport = Arc::new(FakeDohTransport::positive());
        let old_registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            old_transport.clone(),
        )
        .unwrap();
        let latest_registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            latest_transport.clone(),
        )
        .unwrap();
        let old = Arc::new(
            PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, old_registry).unwrap(),
        );
        let latest = Arc::new(
            PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, latest_registry).unwrap(),
        );

        let first_request = request("remote.example.", RecordType::A);
        let qname =
            CanonicalDomain::parse(&first_request.query.question().name().to_ascii()).unwrap();
        let listener_id = ConfigId::new("dns").unwrap();
        let plan = old
            .policy()
            .evaluate(crate::policy::PolicyRequest {
                listener_id: &listener_id,
                doh_route_id: None,
                client_id: None,
                client_addr: first_request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&old, &plan, &first_request).unwrap();
        old.resolve(&first_request).await.unwrap();
        let record = match old
            .cache()
            .lookup(&key, Deadline::new(Instant::now() + Duration::from_secs(1)))
            .await
            .unwrap()
        {
            CacheLookup::Fresh(record) => record,
            other => panic!("expected a fresh cache record, got {other:?}"),
        };
        let now = Instant::now();
        let stale_entry = crate::ports::cache::CacheEntry {
            response: Arc::clone(&record.entry.response),
            upstream: record.entry.upstream.clone(),
            inserted_at: now - Duration::from_secs(1),
            expires_at: now - Duration::from_millis(1),
            stale_until: Some(now + Duration::from_secs(5)),
            response_class: record.entry.response_class,
            producer_revision: record.entry.producer_revision,
            quality: record.entry.quality,
            checksum: record.entry.checksum,
            format_version: record.entry.format_version,
        };
        old.cache()
            .store()
            .compare_and_swap(
                key.clone(),
                crate::ports::cache::CacheCondition::Version(record.version),
                Arc::new(stale_entry),
                Deadline::new(Instant::now() + Duration::from_secs(1)),
            )
            .await
            .unwrap();

        let cell = Arc::new(RuntimeCoreCell::default());
        old.attach_runtime_cell(Arc::clone(&cell));
        latest.attach_runtime_cell(Arc::clone(&cell));
        cell.publish(Some(Arc::new(RuntimeCoreTarget {
            core: Arc::clone(&latest),
            revision: RuntimeRevision(2),
        })));

        let stale = old
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap();
        assert!(matches!(stale, CoreOutcome::Response(_)));
        let mut refreshed = false;
        for _ in 0..100 {
            if latest_transport.calls.load(Ordering::Acquire) >= 1
                && matches!(
                    latest
                        .cache()
                        .lookup(&key, Deadline::new(Instant::now() + Duration::from_secs(1)))
                        .await
                        .unwrap(),
                    CacheLookup::Fresh(_)
                )
            {
                refreshed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            refreshed,
            "latest runtime cache must receive optimistic refresh"
        );
        assert_eq!(old_transport.calls.load(Ordering::Acquire), 1);
        assert_eq!(latest_transport.calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn policy_late_result_sink_publishes_absent_cache_entry() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        let config = Arc::new(config);
        let core = PolicyDnsCore::from_config(config.as_ref(), 42).unwrap();
        let request = request("late.example.", RecordType::A);
        let qname = CanonicalDomain::parse(&request.query.question().name().to_ascii()).unwrap();
        let listener_id = ConfigId::new("dns").unwrap();
        let plan = core
            .policy()
            .evaluate(crate::policy::PolicyRequest {
                listener_id: &listener_id,
                doh_route_id: None,
                client_id: None,
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&core, &plan, &request).expect("cache must be enabled");
        let response =
            CanonicalResponse::empty_response(&request.query, ResponseCode::NoError).unwrap();
        let answer = answer_key(&core, &request).unwrap();
        let sink = core.late_result_sink(&answer, &request, &plan.upstream);
        sink.submit(
            request.query.clone(),
            request.context.clone(),
            UpstreamAttempt {
                attempt_index: 1,
                connector: ConnectorId::new("late").unwrap(),
                outcome: UpstreamOutcome::Response(response),
            },
        );

        let deadline = Deadline::new(Instant::now() + Duration::from_secs(1));
        let mut stored = false;
        for _ in 0..100 {
            if matches!(
                core.cache().lookup(&key, deadline).await.unwrap(),
                CacheLookup::Fresh(_)
            ) {
                stored = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            stored,
            "late response should be published through the finalizer"
        );
        let CacheLookup::Fresh(record) = core.cache().lookup(&key, deadline).await.unwrap() else {
            panic!("late response must remain cached");
        };
        assert_eq!(record.entry.upstream.target_id().as_str(), "remote");
        assert_eq!(
            record.entry.upstream.used_id().map(CacheUpstreamId::as_str),
            Some("late")
        );
    }

    #[tokio::test]
    async fn policy_late_result_sink_promotes_positive_over_early_negative() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        let config = Arc::new(config);
        let transport = Arc::new(FakeDohTransport::new());
        let registry =
            UpstreamRegistry::from_resolved_with_doh_transport(&config.upstreams, transport)
                .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();
        let request = request("late-positive.example.", RecordType::A);
        let qname = CanonicalDomain::parse(&request.query.question().name().to_ascii()).unwrap();
        let listener_id = ConfigId::new("dns").unwrap();
        let plan = core
            .policy()
            .evaluate(crate::policy::PolicyRequest {
                listener_id: &listener_id,
                doh_route_id: None,
                client_id: None,
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&core, &plan, &request).expect("cache must be enabled");
        let early = core.resolve(&request).await.unwrap();
        assert!(matches!(
            early,
            CoreOutcome::Response(response)
                if response.class() == crate::dns::ResponseClass::NoData
        ));

        let answer = answer_key(&core, &request).unwrap();
        let sink = core.late_result_sink(&answer, &request, &plan.upstream);
        let positive = CanonicalResponse::response_with_answers(
            &request.query,
            [Record::from_rdata(
                request.query.question().name().clone(),
                30,
                RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, 10))),
            )],
        )
        .unwrap();
        let expected_positive = positive.clone();
        sink.submit(
            request.query.clone(),
            request.context.clone(),
            UpstreamAttempt {
                attempt_index: 1,
                connector: ConnectorId::new("late-positive").unwrap(),
                outcome: UpstreamOutcome::Response(positive),
            },
        );

        let deadline = Deadline::new(Instant::now() + Duration::from_secs(1));
        let mut promoted = false;
        for _ in 0..100 {
            if let CacheLookup::Fresh(record) = core.cache().lookup(&key, deadline).await.unwrap()
                && record.entry.response.class() == crate::dns::ResponseClass::Positive
            {
                promoted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            promoted,
            "late positive response should replace an early negative cache entry"
        );

        let replacement = CanonicalResponse::response_with_answers(
            &request.query,
            [Record::from_rdata(
                request.query.question().name().clone(),
                30,
                RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, 11))),
            )],
        )
        .unwrap();
        sink.submit(
            request.query.clone(),
            request.context.clone(),
            UpstreamAttempt {
                attempt_index: 2,
                connector: ConnectorId::new("late-positive-replacement").unwrap(),
                outcome: UpstreamOutcome::Response(replacement),
            },
        );
        for _ in 0..100 {
            if core.finalizer_owner().active_tasks() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let final_record = match core.cache().lookup(&key, deadline).await.unwrap() {
            CacheLookup::Fresh(record) => record,
            other => panic!("expected a fresh promoted cache record, got {other:?}"),
        };
        assert_eq!(
            final_record.entry.response.as_ref(),
            &expected_positive,
            "same or lower quality late response must not overwrite the promoted cache entry"
        );
        assert_eq!(
            final_record
                .entry
                .upstream
                .used_id()
                .map(CacheUpstreamId::as_str),
            Some("late-positive")
        );
    }

    #[tokio::test]
    async fn policy_late_result_sink_keeps_negative_quality_stable() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        let config = Arc::new(config);
        let transport = Arc::new(FakeDohTransport::new());
        let registry =
            UpstreamRegistry::from_resolved_with_doh_transport(&config.upstreams, transport)
                .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, registry).unwrap();
        let request = request("late-negative.example.", RecordType::A);
        let qname = CanonicalDomain::parse(&request.query.question().name().to_ascii()).unwrap();
        let listener_id = ConfigId::new("dns").unwrap();
        let plan = core
            .policy()
            .evaluate(crate::policy::PolicyRequest {
                listener_id: &listener_id,
                doh_route_id: None,
                client_id: None,
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&core, &plan, &request).expect("cache must be enabled");
        let early = core.resolve(&request).await.unwrap();
        assert!(matches!(
            early,
            CoreOutcome::Response(response)
                if response.class() == crate::dns::ResponseClass::NoData
        ));

        let answer = answer_key(&core, &request).unwrap();
        let sink = core.late_result_sink(&answer, &request, &plan.upstream);
        for (attempt_index, code) in [(1, ResponseCode::NXDomain), (2, ResponseCode::ServFail)] {
            let response = CanonicalResponse::empty_response(&request.query, code).unwrap();
            sink.submit(
                request.query.clone(),
                request.context.clone(),
                UpstreamAttempt {
                    attempt_index,
                    connector: ConnectorId::new(format!("late-negative-{attempt_index}")).unwrap(),
                    outcome: UpstreamOutcome::Response(response),
                },
            );
        }

        let deadline = Deadline::new(Instant::now() + Duration::from_secs(1));
        for _ in 0..100 {
            if core.finalizer_owner().active_tasks() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let record = match core.cache().lookup(&key, deadline).await.unwrap() {
            CacheLookup::Fresh(record) => record,
            other => panic!("expected a fresh negative cache record, got {other:?}"),
        };
        assert_eq!(
            record.entry.response.class(),
            crate::dns::ResponseClass::NoData,
            "equal-quality Negative and lower-quality Failure must not replace NoData"
        );
    }

    #[tokio::test]
    async fn policy_late_result_sink_routes_to_latest_runtime_cache() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        let config = Arc::new(config);
        let old_registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            Arc::new(FakeDohTransport::new()),
        )
        .unwrap();
        let latest_registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &config.upstreams,
            Arc::new(FakeDohTransport::new()),
        )
        .unwrap();
        let old = Arc::new(
            PolicyDnsCore::from_config_with_registry(config.as_ref(), 42, old_registry).unwrap(),
        );
        let latest = Arc::new(
            PolicyDnsCore::from_config_with_registry(config.as_ref(), 43, latest_registry).unwrap(),
        );
        let request = request("late-latest.example.", RecordType::A);
        let qname = CanonicalDomain::parse(&request.query.question().name().to_ascii()).unwrap();
        let listener_id = ConfigId::new("dns").unwrap();
        let plan = old
            .policy()
            .evaluate(crate::policy::PolicyRequest {
                listener_id: &listener_id,
                doh_route_id: None,
                client_id: None,
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&old, &plan, &request).expect("cache must be enabled");

        let cell = Arc::new(RuntimeCoreCell::default());
        old.attach_runtime_cell(Arc::clone(&cell));
        latest.attach_runtime_cell(Arc::clone(&cell));
        cell.publish(Some(Arc::new(RuntimeCoreTarget {
            core: Arc::clone(&latest),
            revision: RuntimeRevision(43),
        })));

        let response = CanonicalResponse::response_with_answers(
            &request.query,
            [Record::from_rdata(
                request.query.question().name().clone(),
                30,
                RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, 12))),
            )],
        )
        .unwrap();
        let answer = answer_key(&old, &request).unwrap();
        old.late_result_sink(&answer, &request, &plan.upstream)
            .submit(
                request.query.clone(),
                request.context.clone(),
                UpstreamAttempt {
                    attempt_index: 1,
                    connector: ConnectorId::new("late-latest").unwrap(),
                    outcome: UpstreamOutcome::Response(response),
                },
            );

        let deadline = Deadline::new(Instant::now() + Duration::from_secs(1));
        let mut routed = false;
        for _ in 0..100 {
            if let CacheLookup::Fresh(record) = latest.cache().lookup(&key, deadline).await.unwrap()
                && record.entry.response.class() == crate::dns::ResponseClass::Positive
            {
                routed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            routed,
            "late sink should publish into the latest Runtime cache"
        );
        let CacheLookup::Fresh(record) = latest.cache().lookup(&key, deadline).await.unwrap()
        else {
            panic!("latest runtime must retain routed response");
        };
        assert_eq!(record.entry.upstream.target_id().as_str(), "remote");
        assert_eq!(
            record.entry.upstream.used_id().map(CacheUpstreamId::as_str),
            Some("late-latest")
        );
        assert!(matches!(
            old.cache().lookup(&key, deadline).await.unwrap(),
            CacheLookup::Miss
        ));
    }

    // V2-L01：primary lease 提交/丢弃、单 follower 取消、同代/换代与 shutdown 的交错。
    #[tokio::test]
    async fn contract_v2_nested_positive_reload_and_shutdown_keep_response_immutable() {
        use crate::ports::cache::{CacheLoadCompletion, CacheLoadFailure, CacheLoadReservation};

        for commit_primary in [false, true] {
            for switch_revision in [false, true] {
                for shutdown_before_late in [false, true] {
                    let mut config = Arc::try_unwrap(doh_config()).unwrap();
                    config.dns.cache.enabled = true;
                    route_doh_through_nested_single_member_group(&mut config);
                    config.upstreams.push(ResolvedUpstream::Hosts {
                        id: ConfigId::new("fast").unwrap(),
                        format: "hosts".into(),
                        hosts: "192.0.2.10 race.test".into(),
                    });
                    let ResolvedUpstream::Group {
                        upstreams,
                        upstream_mode,
                        timeout,
                        ..
                    } = &mut config.upstreams[1]
                    else {
                        panic!("inner group must exist");
                    };
                    *upstream_mode = crate::config::model::UpstreamMode::Parallel;
                    *timeout = Duration::from_secs(5);
                    upstreams.push(ResolvedUpstreamMember {
                        name: ConfigId::new("fast").unwrap(),
                        weight: 1,
                    });
                    let gate = Arc::new(crate::ports::testing::TestGate::new());
                    let transport = Arc::new(GatedPositiveDohTransport {
                        gate: Arc::clone(&gate),
                    });
                    let old_registry = UpstreamRegistry::from_resolved_with_doh_transport(
                        &super::direct_upstreams(&config.upstreams),
                        transport,
                    )
                    .unwrap();
                    let latest_registry = UpstreamRegistry::from_resolved_with_doh_transport(
                        &super::direct_upstreams(&config.upstreams),
                        Arc::new(FakeDohTransport::new()),
                    )
                    .unwrap();
                    let old = Arc::new(
                        PolicyDnsCore::from_config_with_registry(&config, 42, old_registry)
                            .unwrap(),
                    );
                    let latest = Arc::new(
                        PolicyDnsCore::from_config_with_registry(&config, 42, latest_registry)
                            .unwrap(),
                    );
                    let cell = Arc::new(RuntimeCoreCell::default());
                    old.attach_runtime_cell(Arc::clone(&cell));
                    latest.attach_runtime_cell(Arc::clone(&cell));
                    cell.publish(Some(Arc::new(RuntimeCoreTarget {
                        core: Arc::clone(&old),
                        revision: RuntimeRevision(1),
                    })));
                    let mut request = request("race.test.", RecordType::A);
                    request.context.runtime_revision = RuntimeRevision(1);
                    let completed = tokio::time::timeout(
                        Duration::from_secs(5),
                        old.resolve_with_completion(&request),
                    )
                    .await
                    .unwrap();
                    gate.wait_reached().await;
                    let CoreOutcome::Response(response) = completed.result.unwrap() else {
                        panic!("expected response")
                    };
                    let frozen = response.as_message().clone();
                    assert!(frozen.answers.iter().any(|record| matches!(&record.data,
                RData::A(address) if address.0 == std::net::Ipv4Addr::new(192, 0, 2, 10))));
                    let qname = CanonicalDomain::parse("race.test.").unwrap();
                    let listener = ConfigId::new("dns").unwrap();
                    let plan = old
                        .policy()
                        .evaluate(crate::policy::PolicyRequest {
                            listener_id: &listener,
                            doh_route_id: None,
                            client_id: None,
                            client_addr: request.context.client.client_addr,
                            client_digest: None,
                            qname: Some(&qname),
                        })
                        .unwrap();
                    let key = cache_key(&old, &plan, &request).unwrap();
                    let budget = Deadline::new(Instant::now() + Duration::from_secs(5));
                    let CacheLoadReservation::Follower(waiter) =
                        old.cache().reserve_load(key.clone(), budget).await.unwrap()
                    else {
                        panic!("primary response handoff must still own the single-flight lease");
                    };
                    let CacheLoadReservation::Follower(cancelled_waiter) =
                        old.cache().reserve_load(key.clone(), budget).await.unwrap()
                    else {
                        panic!("second follower must share the same producer");
                    };
                    let follower_cancellation = Cancellation::new();
                    let mut cancelled_wait =
                        old.cache()
                            .wait_load(cancelled_waiter, budget, &follower_cancellation);
                    std::future::poll_fn(|cx| {
                        assert!(cancelled_wait.as_mut().poll(cx).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    follower_cancellation.cancel(crate::dns::CancelReason::ClientDisconnected);
                    let error = cancelled_wait.await.unwrap_err();
                    assert!(matches!(error,
                crate::cache::CacheFacadeError::Store(error)
                if matches!(error.class(), crate::ports::PortErrorClass::Cancelled(crate::dns::CancelReason::ClientDisconnected))));
                    let candidate = completed
                        .cache_commit
                        .expect("primary lease must be handed off");
                    if commit_primary {
                        assert_eq!(
                            candidate.commit(Duration::from_secs(1)).await,
                            crate::cache::CacheCommitOutcome::Stored
                        );
                    } else {
                        drop(candidate);
                    }
                    let completion = old
                        .cache()
                        .wait_load(waiter, budget, &Cancellation::new())
                        .await
                        .unwrap();
                    if commit_primary {
                        assert!(matches!(completion, CacheLoadCompletion::Ready(_)));
                    } else {
                        assert!(matches!(
                            completion,
                            CacheLoadCompletion::Failed(CacheLoadFailure::Abandoned)
                        ));
                    }
                    // late drain 仍被 gate 持有，但 primary lease 已结束，不能延长 single-flight 口径。
                    let CacheLoadReservation::Leader(fresh_lease) =
                        old.cache().reserve_load(key.clone(), budget).await.unwrap()
                    else {
                        panic!("late collection must not retain primary lease");
                    };
                    drop(fresh_lease);
                    let target = if switch_revision { &latest } else { &old };
                    if switch_revision {
                        cell.publish(Some(Arc::new(RuntimeCoreTarget {
                            core: Arc::clone(&latest),
                            revision: RuntimeRevision(2),
                        })));
                    }
                    if shutdown_before_late {
                        assert!(old.finalizer_owner().shutdown_until(budget).await.completed);
                        assert!(
                            latest
                                .finalizer_owner()
                                .shutdown_until(budget)
                                .await
                                .completed
                        );
                        gate.release();
                        let lookup = target.cache().lookup(&key, budget).await.unwrap();
                        if !switch_revision && commit_primary {
                            assert!(matches!(lookup, CacheLookup::Fresh(record)
                        if record.entry.upstream.used_id().unwrap().as_str() == "inner"));
                        } else {
                            assert!(matches!(lookup, CacheLookup::Miss));
                        }
                    } else {
                        gate.release();
                        old.finalizer_owner().wait_idle_for_test().await;
                        latest.finalizer_owner().wait_idle_for_test().await;
                        let CacheLookup::Fresh(record) =
                            target.cache().lookup(&key, budget).await.unwrap()
                        else {
                            panic!("late candidate must reach latest cache");
                        };
                        let keep_primary = !switch_revision && commit_primary;
                        assert_eq!(
                            record.entry.producer_revision,
                            RuntimeRevision(if switch_revision { 2 } else { 1 })
                        );
                        // primary 保存顶层 group member；late attempt 保留实际 connector，不扩成逐 attempt trace。
                        assert_eq!(
                            record.entry.upstream.used_id().unwrap().as_str(),
                            if keep_primary { "inner" } else { "remote" }
                        );
                        assert!(
                    record
                        .entry
                        .response
                        .as_message()
                        .answers
                        .iter()
                        .any(|record| matches!(&record.data,
                    RData::A(address) if address.0 == std::net::Ipv4Addr::new(192, 0, 2, if keep_primary { 10 } else { 20 })))
                );
                        assert!(old.finalizer_owner().shutdown_until(budget).await.completed);
                        assert!(
                            latest
                                .finalizer_owner()
                                .shutdown_until(budget)
                                .await
                                .completed
                        );
                    }
                    assert_eq!(response.as_message(), &frozen);
                    assert_eq!(old.finalizer_owner().active_tasks(), 0);
                    assert_eq!(latest.finalizer_owner().active_tasks(), 0);
                }
            }
        }
    }

    fn config() -> std::sync::Arc<crate::config::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("policy-core");
        ConfigV2Loader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 2
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
  records_path: ./queries
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {{}}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5300
    strategy: default
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "127.0.0.1 upstream.example"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: "192.0.2.10 local.example"
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
"#,
            ))
            .expect("policy core fixture must be valid")
            .resolved
    }

    fn rule_config() -> std::sync::Arc<crate::config::ResolvedConfig> {
        let mut config = Arc::try_unwrap(config()).expect("policy fixture must be unique");
        let resource = ConfigId::new("dynamic-rules").unwrap();
        config.rule_sets.push(ResolvedRuleSet::Const {
            selectors: Default::default(),
            id: resource.clone(),
            format: RuleSetFormat::Clash,
            rule: "DOMAIN-SUFFIX,old.example\n".to_owned(),
        });
        config.strategies[0].rules.insert(
            0,
            ResolvedStrategyRule {
                rule_set: Some(ResolvedRuleSetRef {
                    resource,
                    selector: None,
                }),
                hosts: None,
                upstream: Some(ConfigId::new("local").unwrap()),
                edns_client_subnet: ResolvedEcs {
                    mode: EcsMode::Disabled,
                    custom_ip: None,
                    source: ValueSource::Default,
                },
            },
        );
        Arc::new(config)
    }

    fn doh_config() -> std::sync::Arc<crate::config::ResolvedConfig> {
        doh_config_with_address("http://dns.example.test/dns-query")
    }

    /// 将 DoH fixture 的默认上游改为 outer → inner → remote 的嵌套 group。
    fn route_doh_through_nested_single_member_group(config: &mut crate::config::ResolvedConfig) {
        config.upstreams.push(ResolvedUpstream::Group {
            id: ConfigId::new("inner").unwrap(),
            upstreams: vec![ResolvedUpstreamMember {
                name: ConfigId::new("remote").unwrap(),
                weight: 1,
            }],
            upstream_mode: crate::config::model::UpstreamMode::Failover,
            timeout: Duration::from_secs(1),
            fallbacks: Vec::new(),
            fallback_upstream_mode: None,
            fallback_timeout: None,
        });
        config.upstreams.push(ResolvedUpstream::Group {
            id: ConfigId::new("group").unwrap(),
            upstreams: vec![ResolvedUpstreamMember {
                name: ConfigId::new("inner").unwrap(),
                weight: 1,
            }],
            upstream_mode: crate::config::model::UpstreamMode::Failover,
            timeout: Duration::from_secs(1),
            fallbacks: Vec::new(),
            fallback_upstream_mode: None,
            fallback_timeout: None,
        });
        config.strategies[0].default_upstream = ConfigId::new("group").unwrap();
    }

    fn doh_config_with_address(address: &str) -> std::sync::Arc<crate::config::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("policy-doh");
        let source = format!(
            r#"
version: 2
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
  records_path: ./queries
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {{}}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5302
    strategy: default
upstreams:
  - type: doh
    name: remote
    address: __DOH_ADDRESS__
    connect_ip: 192.0.2.44
strategy:
  - name: default
    rules:
      - hosts: unused-hosts
    default_upstream: remote
hosts:
  - type: const
    name: unused-hosts
    format: hosts
    hosts: "192.0.2.99 unused.example"
        "#
        )
        .replace("__DOH_ADDRESS__", address);
        ConfigV2Loader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("policy DoH fixture must be valid")
            .resolved
    }

    #[test]
    fn policy_core_accepts_configured_plain_http_doh_with_disabled_ecs() {
        let core = PolicyDnsCore::from_config(doh_config().as_ref(), 42).unwrap();
        assert_eq!(core.upstream_count(), 1);
    }

    #[tokio::test]
    async fn policy_core_from_config_executes_proxy_doh_upstream() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();

            let mut connect = [0_u8; 10];
            stream.read_exact(&mut connect).await.unwrap();
            assert_eq!(connect, [5, 1, 0, 1, 192, 0, 2, 44, 0, 80]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();

            let mut bytes = Vec::new();
            let header_end;
            let body_end;
            loop {
                let mut chunk = [0_u8; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let header_end_candidate = end + 4;
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                let content_length = headers
                    .split("\r\n")
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= header_end_candidate + content_length {
                    header_end = header_end_candidate;
                    body_end = header_end_candidate + content_length;
                    assert!(headers.starts_with("POST /dns-query HTTP/1.1\r\n"));
                    assert!(headers.contains("Host: dns.example.test\r\n"));
                    break;
                }
            }

            let request = Message::from_vec(&bytes[header_end..body_end]).unwrap();
            let mut response =
                Message::new(request.metadata.id, MessageType::Response, OpCode::Query);
            response.metadata.response_code = ResponseCode::NoError;
            response.add_query(request.queries[0].clone());
            let response_body = response.to_vec().unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&response_body).await.unwrap();
        });

        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "fluxdns-policy-proxy-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let secret_path = root.join("proxy-url");
        fs::write(&secret_path, format!("socks5://127.0.0.1:{proxy_port}")).unwrap();

        let mut config = Arc::try_unwrap(doh_config()).ok().unwrap();
        config.outbounds.push(ResolvedOutbound {
            id: ConfigId::new("socks").unwrap(),
            kind: crate::config::model::OutboundType::Socks5,
            proxy_url: ResolvedSecretRef {
                env: None,
                file: Some(secret_path),
                url: None,
            },
        });
        let ResolvedUpstream::Doh { proxy, .. } = &mut config.upstreams[0] else {
            panic!("expected DoH upstream");
        };
        *proxy = Some(ConfigId::new("socks").unwrap());

        let core = PolicyDnsCore::from_config(&config, 42).unwrap();
        let CoreOutcome::Response(response) = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected proxied upstream response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::NoData);
        server.await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn group_config() -> std::sync::Arc<crate::config::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("policy-group");
        ConfigV2Loader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 2
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
  records_path: ./queries
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {{}}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5301
    strategy: default
upstreams:
  - type: hosts
    name: first
    format: hosts
    hosts: "192.0.2.11 group.example"
  - type: hosts
    name: second
    format: hosts
    hosts: "192.0.2.12 group.example"
  - type: group
    name: group
    upstreams:
      - name: first
        weight: 1
      - name: second
        weight: 1
    upstream_mode: round-robin
    timeout: 1s
hosts:
  - type: const
    name: unused-hosts
    format: hosts
    hosts: "192.0.2.99 unused.example"
strategy:
  - name: default
    rules:
      - hosts: unused-hosts
    default_upstream: group
"#,
            ))
            .expect("policy group fixture must be valid")
            .resolved
    }

    fn request(name: &str, record_type: RecordType) -> DnsRequest {
        let mut message = Message::new(7, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(Name::from_str(name).unwrap(), record_type));
        let query = CanonicalQuery::from_message(message).unwrap();
        let now = Instant::now();
        DnsRequest {
            query,
            context: RequestContext {
                meta: RequestMeta {
                    completion: Default::default(),
                    request_id: RequestId(1),
                    trace_id: None,
                    received_at: now,
                    received_at_utc: SystemTime::now(),
                    deadline: Deadline::new(now + Duration::from_secs(30)),
                    cancellation: Cancellation::new(),
                    connection_id: None,
                    stream_id: None,
                    listener_id: ListenerId::from("dns"),
                    route_id: None,
                    original_dns_id: Some(7),
                },
                client: crate::dns::ClientIdentity {
                    peer_addr: Some(SocketAddr::from(([127, 0, 0, 1], 5300))),
                    client_addr: Some(IpAddr::from([127, 0, 0, 1])),
                    client_id: None,
                },
                transport: TransportCapabilities {
                    class: TransportClass::Datagram,
                    cache_compatibility: CacheCompatibilityKey(1),
                },
                runtime_revision: RuntimeRevision(1),
            },
        }
    }

    #[tokio::test]
    async fn policy_hosts_rule_produces_local_answer_and_nodata() {
        let core = PolicyDnsCore::from_config(config().as_ref(), 42).unwrap();
        assert_eq!(core.host_resource_count(), 1);
        assert_eq!(core.upstream_count(), 1);

        let (answer, observation) = core
            .resolve_with_observation(&request("local.example.", RecordType::A))
            .await;
        let answer = answer.unwrap();
        let CoreOutcome::Response(answer) = answer else {
            panic!("expected local response");
        };
        assert_eq!(answer.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(answer.ttl().min_ttl, Some(42));
        let matched = observation
            .expect("hosts response must include observation")
            .matched_rule
            .expect("hosts response must include matched rule");
        assert_eq!(matched.source, MatchedRuleSource::StrategyHosts);
        assert_eq!(matched.resource_id.as_ref(), "local-hosts");
        assert_eq!(matched.resource_version, Some(ResourceVersion::new(1, 1)));
        assert_eq!(matched.ordinal, Some(0));

        let nodata = core
            .resolve(&request("local.example.", RecordType::AAAA))
            .await
            .unwrap();
        let CoreOutcome::Response(nodata) = nodata else {
            panic!("expected nodata response");
        };
        assert_eq!(nodata.class(), crate::dns::ResponseClass::NoData);
    }

    #[tokio::test]
    async fn policy_observation_reports_rule_set_resource_without_matcher_content() {
        let core = PolicyDnsCore::from_config(rule_config().as_ref(), 42).unwrap();

        let (_, observation) = core
            .resolve_with_observation(&request("old.example.", RecordType::A))
            .await;

        let matched = observation
            .expect("rule-set response must include observation")
            .matched_rule
            .expect("rule-set response must include matched rule");
        assert_eq!(matched.source, MatchedRuleSource::RuleSet);
        assert_eq!(matched.resource_id.as_ref(), "dynamic-rules");
        assert_eq!(matched.resource_version, Some(ResourceVersion::new(1, 1)));
        assert_eq!(matched.ordinal, Some(0));
    }

    #[tokio::test]
    async fn policy_applies_selected_ttl_override_to_hosts_and_upstream_answers() {
        let mut config = Arc::try_unwrap(config()).unwrap();
        config.dns.cache.enabled = true;
        config.strategies[0].ttl_override = ResolvedTtlOverride {
            enabled: true,
            min: Some(Duration::from_secs(50)),
            max: Some(Duration::from_secs(50)),
            source: ValueSource::Strategy,
        };
        let core = PolicyDnsCore::from_config(&config, 42).unwrap();

        let CoreOutcome::Response(local) = core
            .resolve(&request("local.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected local policy response");
        };
        assert_eq!(local.ttl().min_ttl, Some(50));

        let upstream_request = request("upstream.example.", RecordType::A);
        let qname =
            CanonicalDomain::parse(&upstream_request.query.question().name().to_ascii()).unwrap();
        let plan = core
            .policy()
            .evaluate(crate::policy::PolicyRequest {
                listener_id: &ConfigId::new("dns").unwrap(),
                doh_route_id: None,
                client_id: None,
                client_addr: upstream_request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&core, &plan, &upstream_request).unwrap();
        let CoreOutcome::Response(upstream) = core.resolve(&upstream_request).await.unwrap() else {
            panic!("expected upstream policy response");
        };
        assert_eq!(upstream.ttl().min_ttl, Some(50));

        let CacheLookup::Fresh(stored) = core
            .cache()
            .lookup(&key, upstream_request.context.meta.deadline)
            .await
            .unwrap()
        else {
            panic!("expected cached origin response");
        };
        assert_eq!(
            stored.entry.response.ttl().min_ttl,
            Some(crate::dns::DEFAULT_LOCAL_TTL)
        );
    }

    #[tokio::test]
    async fn policy_core_publishes_new_rule_set_snapshot_and_rejects_stale_version() {
        let core = PolicyDnsCore::from_config(rule_config().as_ref(), 42).unwrap();
        let evaluate = |name: &str| {
            let request = request(name, RecordType::A);
            let qname =
                CanonicalDomain::parse(&request.query.question().name().to_ascii()).unwrap();
            core.policy()
                .evaluate(crate::policy::PolicyRequest {
                    listener_id: &ConfigId::new("dns").unwrap(),
                    doh_route_id: None,
                    client_id: None,
                    client_addr: request.context.client.client_addr,
                    client_digest: None,
                    qname: Some(&qname),
                })
                .unwrap()
        };

        assert!(evaluate("new.example.").matched_rule.is_none());
        assert!(evaluate("old.example.").matched_rule.is_some());

        let resource = ConfigId::new("dynamic-rules").unwrap();
        let index = RuleIndex::parse("DOMAIN-SUFFIX,new.example\n", RuleSetFormat::Clash).unwrap();
        let snapshot = ResourceSnapshot::new(
            resource.clone(),
            2,
            1,
            "hash-new",
            "fingerprint-new",
            "rule-index-v1",
            SystemTime::UNIX_EPOCH,
            ResourceSourceKind::Remote,
            false,
            ResourceStaleStatus::Fresh,
            index.clone(),
        );
        core.publish_rule_set_resource(snapshot).unwrap();

        assert!(evaluate("new.example.").matched_rule.is_some());
        assert!(evaluate("old.example.").matched_rule.is_none());
        let (_, observation) = core
            .resolve_with_observation(&request("new.example.", RecordType::A))
            .await;
        assert_eq!(
            observation
                .and_then(|value| value.matched_rule)
                .and_then(|matched| matched.resource_version),
            Some(ResourceVersion::new(2, 1))
        );

        let stale = ResourceSnapshot::new(
            resource,
            1,
            1,
            "hash-old",
            "fingerprint-old",
            "rule-index-v1",
            SystemTime::UNIX_EPOCH,
            ResourceSourceKind::Remote,
            false,
            ResourceStaleStatus::Fresh,
            index,
        );
        assert!(matches!(
            core.publish_rule_set_resource(stale),
            Err(super::PolicyResourcePublishError::StaleVersion { .. })
        ));
    }

    #[tokio::test]
    async fn policy_core_publishes_new_hosts_snapshot() {
        let mut config = Arc::try_unwrap(config()).unwrap();
        config.dns.cache.enabled = true;
        let core = PolicyDnsCore::from_config(&config, 42).unwrap();
        let CoreOutcome::Response(before_refresh) = core
            .resolve(&request("updated.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected upstream response before hosts refresh");
        };
        assert_eq!(before_refresh.class(), crate::dns::ResponseClass::NxDomain);

        let index =
            crate::resource::HostsIndex::parse_hosts("192.0.2.20 updated.example\n").unwrap();
        let snapshot = ResourceSnapshot::new(
            ConfigId::new("local-hosts").unwrap(),
            2,
            1,
            "hash-updated",
            "fingerprint-updated",
            "hosts-index-v1",
            SystemTime::UNIX_EPOCH,
            ResourceSourceKind::File,
            false,
            ResourceStaleStatus::Fresh,
            index,
        );
        core.publish_hosts_resource(snapshot).unwrap();

        let CoreOutcome::Response(updated) = core
            .resolve(&request("updated.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected updated hosts response");
        };
        assert_eq!(updated.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(
            core.cache().store().stats().hits,
            0,
            "matching the refreshed hosts must answer locally without reading the upstream entry"
        );

        let CoreOutcome::Response(previous) = core
            .resolve(&request("local.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected previous hosts response");
        };
        assert_eq!(previous.class(), crate::dns::ResponseClass::NxDomain);
    }

    #[test]
    fn answer_key_depends_only_on_target_subtree_content() {
        let key = |mut config: crate::config::ResolvedConfig| {
            config.dns.cache.enabled = true;
            let core = PolicyDnsCore::from_config(&config, 42).unwrap();
            answer_key(&core, &request("remote.example.", RecordType::A))
                .expect("cache must be enabled")
        };
        let original = key(Arc::try_unwrap(config()).unwrap());

        let mut unrelated = Arc::try_unwrap(config()).unwrap();
        unrelated.logs.enable = !unrelated.logs.enable;
        unrelated.database.path.push("other.sqlite3");
        unrelated.upstreams.push(ResolvedUpstream::Hosts {
            id: ConfigId::new("unrelated").unwrap(),
            format: "hosts".into(),
            hosts: "192.0.2.40 unrelated.example".into(),
        });
        assert_eq!(key(unrelated), original);

        let mut target = Arc::try_unwrap(config()).unwrap();
        let ResolvedUpstream::Hosts { hosts, .. } = &mut target.upstreams[0] else {
            panic!("fixture must contain a hosts upstream");
        };
        hosts.push_str("\n192.0.2.30 changed.example");
        assert_ne!(key(target), original);
    }

    #[tokio::test]
    async fn policy_without_local_match_uses_supported_upstream() {
        let core = PolicyDnsCore::from_config(config().as_ref(), 42).unwrap();
        let response = core
            .resolve(&request("remote.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(response) = response else {
            panic!("expected upstream response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::NxDomain);
    }

    #[tokio::test]
    async fn policy_executes_group_with_supported_hosts_members() {
        let mut config = Arc::try_unwrap(group_config()).unwrap();
        config.dns.cache.enabled = true;
        let core = PolicyDnsCore::from_config(&config, 42).unwrap();
        assert_eq!(core.host_resource_count(), 1);
        assert_eq!(core.upstream_count(), 3);

        let (response, observation) = core
            .resolve_with_observation(&request("group.example.", RecordType::A))
            .await;
        let response = response.unwrap();
        let CoreOutcome::Response(response) = response else {
            panic!("expected group response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(response.ttl().min_ttl, Some(crate::dns::DEFAULT_LOCAL_TTL));
        let observation = observation.expect("group response must include observation");
        assert_eq!(observation.upstream_id.as_deref(), Some("group"));
        assert_eq!(observation.upstream_member_id.as_deref(), Some("first"));
        assert_eq!(observation.upstream_used_id.as_deref(), Some("first"));

        let (_, cached_observation) = core
            .resolve_with_observation(&request("group.example.", RecordType::A))
            .await;
        let cached_observation =
            cached_observation.expect("cache hit must preserve group provenance");
        assert_eq!(
            cached_observation.source,
            crate::ports::storage::StatsSource::Cache
        );
        assert_eq!(cached_observation.upstream_id.as_deref(), Some("group"));
        assert_eq!(
            cached_observation.upstream_member_id.as_deref(),
            Some("first")
        );
        assert_eq!(
            cached_observation.upstream_used_id.as_deref(),
            Some("first")
        );
    }

    #[tokio::test]
    async fn upstream_runtime_executes_nested_groups() {
        let local = ResolvedUpstream::Hosts {
            id: ConfigId::new("local").unwrap(),
            format: "hosts".to_owned(),
            hosts: "192.0.2.11 nested.example\n".to_owned(),
        };
        let inner = ResolvedUpstream::Group {
            id: ConfigId::new("inner").unwrap(),
            upstreams: vec![ResolvedUpstreamMember {
                name: ConfigId::new("local").unwrap(),
                weight: 1,
            }],
            upstream_mode: crate::config::model::UpstreamMode::Failover,
            timeout: Duration::from_secs(1),
            fallbacks: Vec::new(),
            fallback_upstream_mode: None,
            fallback_timeout: None,
        };
        let outer = ResolvedUpstream::Group {
            id: ConfigId::new("outer").unwrap(),
            upstreams: vec![ResolvedUpstreamMember {
                name: ConfigId::new("inner").unwrap(),
                weight: 1,
            }],
            upstream_mode: crate::config::model::UpstreamMode::Failover,
            timeout: Duration::from_secs(1),
            fallbacks: Vec::new(),
            fallback_upstream_mode: None,
            fallback_timeout: None,
        };
        let registry = UpstreamRegistry::from_resolved(std::slice::from_ref(&local)).unwrap();
        let runtime = UpstreamRuntime::from_registry(&[local, inner, outer], registry).unwrap();
        let request = request("nested.example.", RecordType::A);
        let outcome = runtime
            .exchange(
                &ConfigId::new("outer").unwrap(),
                &request.query,
                &request.context,
                None,
                None,
            )
            .await
            .expect("nested group must resolve");
        assert_eq!(outcome.target_id.as_ref(), "outer");
        assert_eq!(outcome.used_id.as_deref(), Some("inner"));
        assert!(matches!(
            &outcome.outcome,
            UpstreamOutcome::Response(response)
                if response.class() == crate::dns::ResponseClass::Positive
        ));
    }

    #[test]
    fn upstream_runtime_rejects_nested_group_cycle() {
        let group_a = ResolvedUpstream::Group {
            id: ConfigId::new("group-a").unwrap(),
            upstreams: vec![ResolvedUpstreamMember {
                name: ConfigId::new("group-b").unwrap(),
                weight: 1,
            }],
            upstream_mode: crate::config::model::UpstreamMode::Failover,
            timeout: Duration::from_secs(1),
            fallbacks: Vec::new(),
            fallback_upstream_mode: None,
            fallback_timeout: None,
        };
        let group_b = ResolvedUpstream::Group {
            id: ConfigId::new("group-b").unwrap(),
            upstreams: vec![ResolvedUpstreamMember {
                name: ConfigId::new("group-a").unwrap(),
                weight: 1,
            }],
            upstream_mode: crate::config::model::UpstreamMode::Failover,
            timeout: Duration::from_secs(1),
            fallbacks: Vec::new(),
            fallback_upstream_mode: None,
            fallback_timeout: None,
        };
        let error = UpstreamRuntime::from_registry(
            &[group_a, group_b],
            UpstreamRegistry::from_resolved(&[]).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.upstream, "group-a");
        assert_eq!(error.reason, "nested group cycle");
    }

    #[tokio::test]
    async fn policy_executes_fallback_after_primary_servfail() {
        let work_path = crate::config::test_support::absolute_path("policy-fallback");
        let config = ConfigV2Loader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 2
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
  records_path: ./queries
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {{}}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5302
    strategy: default
upstreams:
  - type: doh
    name: primary
    address: http://dns.example.test/dns-query
    connect_ip: 192.0.2.44
  - type: hosts
    name: fallback
    format: hosts
    hosts: "192.0.2.12 fallback.example"
  - type: group
    name: group
    upstreams:
      - name: primary
        weight: 1
    upstream_mode: failover
    timeout: 1s
    fallbacks:
      - name: fallback
        weight: 1
    fallback_upstream_mode: failover
    fallback_timeout: 1s
hosts:
  - type: const
    name: unused-hosts
    format: hosts
    hosts: "192.0.2.99 unused.example"
strategy:
  - name: default
    rules:
      - hosts: unused-hosts
    default_upstream: group
"#,
            ))
            .expect("policy fallback fixture must be valid")
            .resolved;
        let direct = super::direct_upstreams(&config.upstreams);
        let registry = UpstreamRegistry::from_resolved_with_doh_transport(
            &direct,
            Arc::new(ServFailDohTransport),
        )
        .unwrap();
        let core = PolicyDnsCore::from_config_with_registry(&config, 42, registry).unwrap();

        let CoreOutcome::Response(response) = core
            .resolve(&request("fallback.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected fallback response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(response.ttl().min_ttl, Some(crate::dns::DEFAULT_LOCAL_TTL));
    }
}
