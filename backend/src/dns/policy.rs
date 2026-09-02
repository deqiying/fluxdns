//! PolicyIndex 驱动的 DNS Core 首轮接线。
//!
//! 本 core 先处理已编译的本地 hosts，再执行当前已支持的 hosts/group upstream。
//! 尚未具备真实 connector 的分支保持确定性的 SERVFAIL，不伪造网络结果。

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use hickory_proto::op::ResponseCode;
use thiserror::Error;

use crate::cache::{
    CacheAdmissionPolicy, CacheFacade, CacheFacadeOptions, CacheFingerprint, CacheKeyDimensions,
    CacheLookup, CacheWriteRequest, CacheWriteResult, LateCacheFinalizer, MemoryCacheStore,
    build_cache_key,
};
use crate::config::resolve::{ConfigId, ResolvedConfig, ResolvedHostsResource, ResolvedUpstream};
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::policy::{PolicyBuildError, PolicyIndex, PolicyRequest};
use crate::ports::PortFuture;
use crate::ports::cache::{CacheLoadCompletion, CacheLoadFailure, CacheLoadReservation};
use crate::ports::exchange::{
    ConnectorId, DnsExchange, TransportFailure, TransportFailureClass, UpstreamOutcome,
};
use crate::resource::{
    CanonicalDomain, HostsIndex, ResourceLoadError, ResourceSnapshot, ResourceVersion, RuleIndex,
};
use crate::upstream::{
    GroupSelector, LateResultSink, RegistryError, UpstreamAttempt, UpstreamGroupExecutor,
    UpstreamRegistry,
};

use super::handler::resource_answers;
use super::{CanonicalResponse, CoreError, CoreOutcome, DnsCore, DnsRequest};

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
        let (cache, late_cache_finalizer) = build_cache_facade(config)?;
        let policy_state = PolicyState {
            index: policy,
            host_versions: resource_versions(&config.hosts),
            rule_set_versions: rule_set_versions(&config.rule_sets, rule_snapshots),
        };
        Ok(Self {
            policy: Arc::new(ArcSwap::from_pointee(policy_state)),
            upstreams,
            cache,
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

struct PolicyLateResultSink {
    cache: Arc<CacheFacade>,
    finalizer: Arc<LateCacheFinalizer>,
    runtime_cell: Arc<ArcSwap<RuntimeCoreCell>>,
    key: crate::ports::cache::CacheKey,
    producer_revision: crate::dns::RuntimeRevision,
    format_version: u16,
    deadline: Deadline,
}

impl LateResultSink for PolicyLateResultSink {
    fn submit(
        &self,
        query: crate::dns::CanonicalQuery,
        _context: crate::dns::RequestContext,
        attempt: UpstreamAttempt,
    ) {
        let crate::ports::exchange::UpstreamOutcome::Response(response) = attempt.outcome else {
            return;
        };
        if !response.matches_query(&query) {
            return;
        }
        let target = self.runtime_cell.load().current();
        let (cache, finalizer, producer_revision) = target.map_or_else(
            || {
                (
                    Arc::clone(&self.cache),
                    Arc::clone(&self.finalizer),
                    self.producer_revision,
                )
            },
            |target| {
                (
                    Arc::clone(&target.core.cache),
                    Arc::clone(&target.core.late_cache_finalizer),
                    target.revision,
                )
            },
        );
        let _ = finalizer.submit(
            cache,
            CacheWriteRequest {
                key: self.key.clone(),
                condition: crate::ports::cache::CacheCondition::Absent,
                response: Arc::new(response),
                now: Instant::now(),
                producer_revision,
                format_version: self.format_version,
                deadline: self.deadline,
            },
        );
    }
}

impl DnsCore for PolicyDnsCore {
    fn resolve<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, Result<CoreOutcome, CoreError>> {
        Box::pin(async move {
            let meta = &request.context.meta;
            if meta.cancellation.is_cancelled() || meta.deadline.is_expired(Instant::now()) {
                return Ok(CoreOutcome::NoResponse);
            }

            let Some(listener_id) = ConfigId::new(meta.listener_id.as_ref().to_owned()).ok() else {
                return servfail(request);
            };
            let qname = match CanonicalDomain::parse(&request.query.question().name().to_ascii()) {
                Ok(qname) => qname,
                Err(_) => return servfail(request),
            };
            let policy = self.policy.load();
            let doh_path = reconstructed_doh_path(request);
            let plan = match policy.index.evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: doh_path.as_deref(),
                client_id: request
                    .context
                    .client
                    .client_id
                    .as_ref()
                    .map(|client_id| client_id.as_str()),
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            }) {
                Ok(plan) => plan,
                Err(_error) => return servfail(request),
            };

            if let Some(resource_id) = plan.hosts {
                let Some(index) = policy.index.hosts_index(&resource_id) else {
                    return servfail(request);
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
                return response
                    .map(CoreOutcome::Response)
                    .map_err(CoreError::ResponseConstruction);
            }

            let Some(outcome) = self.resolve_upstream(request, &plan).await else {
                return servfail(request);
            };
            match outcome {
                UpstreamOutcome::Response(response) if response.matches_query(&request.query) => {
                    Ok(CoreOutcome::Response(response))
                }
                UpstreamOutcome::Cancelled(_) => Ok(CoreOutcome::NoResponse),
                UpstreamOutcome::Response(_) | UpstreamOutcome::TransportFailure(_) => {
                    servfail(request)
                }
            }
        })
    }
}

impl PolicyDnsCore {
    async fn resolve_upstream(
        &self,
        request: &DnsRequest,
        plan: &crate::policy::ResolutionPlan,
    ) -> Option<UpstreamOutcome> {
        let Some(key) = cache_key(plan, request) else {
            return self
                .upstreams
                .exchange(&plan.upstream, &request.query, &request.context, None)
                .await;
        };
        let late_sink = self.late_result_sink(&key, request);
        let deadline = request.context.meta.deadline;
        match self.cache.lookup(&key, deadline).await {
            Ok(CacheLookup::Fresh(record)) => {
                return Some(UpstreamOutcome::Response((*record.entry.response).clone()));
            }
            Ok(CacheLookup::Stale { record, refresh })
                if matches!(
                    &plan.cache,
                    crate::policy::CacheDecision::Pool {
                        optimistic: true,
                        ..
                    }
                ) =>
            {
                let stale_response = Arc::clone(&record.entry.response);
                if refresh.try_consume() {
                    self.schedule_optimistic_refresh(key.clone(), refresh.version(), request, plan);
                }
                return Some(UpstreamOutcome::Response((*stale_response).clone()));
            }
            Ok(CacheLookup::Disabled)
            | Ok(CacheLookup::Miss)
            | Ok(CacheLookup::Stale { .. })
            | Ok(CacheLookup::StoreUnavailable)
            | Err(_) => {}
        }

        let reservation = match self.cache.reserve_load(key.clone(), deadline).await {
            Ok(reservation) => reservation,
            Err(_) => {
                return self
                    .upstreams
                    .exchange(
                        &plan.upstream,
                        &request.query,
                        &request.context,
                        Some(Arc::clone(&late_sink)),
                    )
                    .await;
            }
        };
        match reservation {
            CacheLoadReservation::Follower(waiter) => {
                match self
                    .cache
                    .wait_load(waiter, deadline, &request.context.meta.cancellation)
                    .await
                {
                    Ok(CacheLoadCompletion::Ready(record)) => {
                        Some(UpstreamOutcome::Response((*record.entry.response).clone()))
                    }
                    Ok(CacheLoadCompletion::Failed(CacheLoadFailure::Cancelled(reason))) => {
                        Some(UpstreamOutcome::Cancelled(reason))
                    }
                    Ok(CacheLoadCompletion::Miss) | Ok(CacheLoadCompletion::Failed(_)) | Err(_) => {
                        self.upstreams
                            .exchange(
                                &plan.upstream,
                                &request.query,
                                &request.context,
                                Some(Arc::clone(&late_sink)),
                            )
                            .await
                    }
                }
            }
            CacheLoadReservation::Leader(lease) => {
                let Some(outcome) = self
                    .upstreams
                    .exchange(
                        &plan.upstream,
                        &request.query,
                        &request.context,
                        Some(Arc::clone(&late_sink)),
                    )
                    .await
                else {
                    let _ = self
                        .cache
                        .abandon_load(lease, CacheLoadFailure::Internal, deadline)
                        .await;
                    return None;
                };
                match outcome {
                    UpstreamOutcome::Response(response) => {
                        if !response.matches_query(&request.query) {
                            let _ = self
                                .cache
                                .publish_load(lease, CacheLoadCompletion::Miss, deadline)
                                .await;
                            return Some(UpstreamOutcome::Response(response));
                        }
                        let response_for_cache = Arc::new(response.clone());
                        let write = self
                            .cache
                            .write_response(CacheWriteRequest {
                                key: key.clone(),
                                condition: crate::ports::cache::CacheCondition::Absent,
                                response: response_for_cache,
                                now: Instant::now(),
                                producer_revision: request.context.runtime_revision,
                                format_version: key.format_version,
                                deadline,
                            })
                            .await;
                        let completion = match write {
                            Ok(CacheWriteResult::Stored(_)) => self
                                .cache
                                .store()
                                .get(&key, deadline)
                                .await
                                .ok()
                                .flatten()
                                .map_or(CacheLoadCompletion::Miss, CacheLoadCompletion::Ready),
                            Ok(CacheWriteResult::Rejected(_)) | Err(_) => CacheLoadCompletion::Miss,
                        };
                        let _ = self.cache.publish_load(lease, completion, deadline).await;
                        Some(UpstreamOutcome::Response(response))
                    }
                    UpstreamOutcome::Cancelled(reason) => {
                        let _ = self
                            .cache
                            .abandon_load(lease, CacheLoadFailure::Cancelled(reason), deadline)
                            .await;
                        Some(UpstreamOutcome::Cancelled(reason))
                    }
                    UpstreamOutcome::TransportFailure(failure) => {
                        let _ = self
                            .cache
                            .abandon_load(lease, CacheLoadFailure::Unavailable, deadline)
                            .await;
                        Some(UpstreamOutcome::TransportFailure(failure))
                    }
                }
            }
        }
    }

    fn schedule_optimistic_refresh(
        &self,
        key: crate::ports::cache::CacheKey,
        stale_version: crate::ports::cache::CacheVersion,
        request: &DnsRequest,
        plan: &crate::policy::ResolutionPlan,
    ) {
        let query = request.query.clone();
        let context = optimistic_refresh_context(&request.context);
        let upstream = plan.upstream.clone();
        let target = self.latest_runtime_target();
        let (upstreams, cache, finalizer, producer_revision, condition) = target.map_or_else(
            || {
                (
                    self.upstreams.clone(),
                    Arc::clone(&self.cache),
                    Arc::clone(&self.late_cache_finalizer),
                    request.context.runtime_revision,
                    crate::ports::cache::CacheCondition::Version(stale_version),
                )
            },
            |target| {
                (
                    target.core.upstreams.clone(),
                    Arc::clone(&target.core.cache),
                    Arc::clone(&target.core.late_cache_finalizer),
                    target.revision,
                    crate::ports::cache::CacheCondition::Absent,
                )
            },
        );
        let format_version = key.format_version;
        let deadline = context.meta.deadline;
        let _ = finalizer.submit_task(async move {
            let Some(UpstreamOutcome::Response(response)) =
                upstreams.exchange(&upstream, &query, &context, None).await
            else {
                return;
            };
            if !response.matches_query(&query) {
                return;
            }
            let _ = cache
                .write_response(CacheWriteRequest {
                    key,
                    condition,
                    response: Arc::new(response),
                    now: Instant::now(),
                    producer_revision,
                    format_version,
                    deadline,
                })
                .await;
        });
    }

    fn late_result_sink(
        &self,
        key: &crate::ports::cache::CacheKey,
        request: &DnsRequest,
    ) -> Arc<dyn LateResultSink> {
        Arc::new(PolicyLateResultSink {
            cache: Arc::clone(&self.cache),
            finalizer: Arc::clone(&self.late_cache_finalizer),
            runtime_cell: Arc::clone(&self.runtime_cell),
            key: key.clone(),
            producer_revision: request.context.runtime_revision,
            format_version: key.format_version,
            deadline: Deadline::new(
                Instant::now() + Duration::from_secs(OPTIMISTIC_REFRESH_TIMEOUT_SECS),
            ),
        })
    }
}

const DEFAULT_LATE_CACHE_FINALIZER_CAPACITY: usize = 64;
const OPTIMISTIC_REFRESH_TIMEOUT_SECS: u64 = 2;

fn build_cache_facade(
    config: &ResolvedConfig,
) -> Result<(Arc<CacheFacade>, Arc<LateCacheFinalizer>), PolicyCoreBuildError> {
    let store = MemoryCacheStore::with_max_weight(config.dns.cache.memory_max_size_bytes).map_err(
        |error| PolicyCoreBuildError::Cache {
            reason: error.to_string(),
        },
    )?;
    let options = CacheFacadeOptions {
        enabled: true,
        optimistic_enabled: true,
        admission: CacheAdmissionPolicy::new(
            config.dns.cache.failure_ttl,
            Some(config.dns.cache.optimistic.max_age),
        ),
    };
    let finalizer =
        LateCacheFinalizer::new(DEFAULT_LATE_CACHE_FINALIZER_CAPACITY).map_err(|error| {
            PolicyCoreBuildError::Cache {
                reason: format!("{error:?}"),
            }
        })?;
    Ok((
        Arc::new(CacheFacade::new(Arc::new(store), options)),
        Arc::new(finalizer),
    ))
}

fn optimistic_refresh_context(context: &crate::dns::RequestContext) -> crate::dns::RequestContext {
    let mut refresh = context.clone();
    refresh.meta.deadline = Deadline::new(
        Instant::now() + std::time::Duration::from_secs(OPTIMISTIC_REFRESH_TIMEOUT_SECS),
    );
    refresh.meta.cancellation = Cancellation::new();
    refresh
}

fn cache_key(
    plan: &crate::policy::ResolutionPlan,
    request: &DnsRequest,
) -> Option<crate::ports::cache::CacheKey> {
    let namespace = plan.cache.namespace()?.clone();
    let ecs = format!(
        "{:?}:{:?}",
        plan.edns_client_subnet.mode, plan.edns_client_subnet.custom_ip
    );
    build_cache_key(
        namespace,
        &request.query,
        request.context.transport.cache_compatibility,
        CacheKeyDimensions {
            policy: Some(cache_fingerprint(plan.strategy.id.as_str().as_bytes())),
            target: Some(cache_fingerprint(plan.upstream.as_str().as_bytes())),
            ecs: Some(cache_fingerprint(ecs.as_bytes())),
        },
    )
    .ok()
}

fn cache_fingerprint(input: &[u8]) -> CacheFingerprint {
    let mut digest = [0_u8; 32];
    for index in 0..4 {
        let mut hash = 0xcbf29ce484222325_u64 ^ (index as u64);
        for byte in input {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3_u64);
        }
        let start = index * 8;
        digest[start..start + 8].copy_from_slice(&hash.to_be_bytes());
    }
    CacheFingerprint::from_digest(digest)
}

#[derive(Clone)]
struct UpstreamRuntime {
    direct: BTreeMap<ConfigId, Arc<dyn DnsExchange>>,
    groups: BTreeMap<ConfigId, Arc<UpstreamGroupExecutor>>,
    all: BTreeMap<ConfigId, Arc<dyn DnsExchange>>,
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

        Ok(Self {
            direct,
            groups,
            all,
        })
    }

    fn len(&self) -> usize {
        self.direct.len() + self.groups.len()
    }

    async fn exchange(
        &self,
        upstream: &ConfigId,
        query: &super::CanonicalQuery,
        context: &super::RequestContext,
        late_sink: Option<Arc<dyn LateResultSink>>,
    ) -> Option<UpstreamOutcome> {
        if let Some(exchange) = self.direct.get(upstream) {
            return Some(exchange.exchange(query, context).await);
        }
        if let Some(executor) = self.groups.get(upstream) {
            return match late_sink {
                Some(sink) => executor
                    .execute_with_late_sink(query, context, sink)
                    .await
                    .ok(),
                None => executor.execute(query, context).await.ok(),
            };
        }
        if let Some(exchange) = self.all.get(upstream) {
            return Some(exchange.exchange(query, context).await);
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
            UpstreamGroupExecutor::new_with_timeout(selector, exchanges, *timeout)
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
            UpstreamGroupExecutor::new_with_fallback(
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
) -> Result<Vec<Arc<dyn DnsExchange>>, UpstreamRuntimeBuildError> {
    members
        .iter()
        .map(|member| {
            if let Some(exchange) = all.get(&member.name) {
                return Ok(Arc::clone(exchange));
            }
            if matches!(
                definitions.get(&member.name),
                Some(ResolvedUpstream::Group { .. })
            ) {
                build_group_executor(&member.name, definitions, all, groups, building)?;
                return all.get(&member.name).cloned().ok_or_else(|| {
                    group_build_error(group, "nested group connector is missing".to_owned())
                });
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

fn reconstructed_doh_path(request: &DnsRequest) -> Option<String> {
    let route = request.context.meta.route_id.as_ref()?;
    let mut path = route.as_ref().to_owned();
    if path.contains("{client_id}") {
        let client_id = request.context.client.client_id.as_ref()?;
        path = path.replace("{client_id}", client_id.as_str());
    }
    Some(path)
}

fn servfail(request: &DnsRequest) -> Result<CoreOutcome, CoreError> {
    CanonicalResponse::empty_response(&request.query, ResponseCode::ServFail)
        .map(CoreOutcome::Response)
        .map_err(CoreError::ResponseConstruction)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use crate::cache::CacheLookup;
    use crate::config::model::{EcsMode, RuleSetFormat};
    use crate::config::resolve::{
        ConfigId, ResolvedEcs, ResolvedOutbound, ResolvedRuleSet, ResolvedRuleSetRef,
        ResolvedSecretRef, ResolvedStrategyRule, ResolvedUpstream, ResolvedUpstreamMember,
        ValueSource,
    };
    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, CanonicalResponse, CoreOutcome,
        Deadline, DnsCore, DnsRequest, ListenerId, RequestContext, RequestId, RequestMeta,
        RuntimeRevision, TransportCapabilities, TransportClass,
    };
    use crate::ports::exchange::{ConnectorId, UpstreamOutcome};
    use crate::ports::{PortError, PortFuture};
    use crate::resource::{
        CanonicalDomain, ResourceSnapshot, ResourceSourceKind, ResourceStaleStatus, RuleIndex,
    };
    use crate::upstream::{
        DohHttpRequest, DohHttpResponseOwned, DohHttpTransport, UpstreamAttempt,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{PolicyDnsCore, RuntimeCoreCell, RuntimeCoreTarget, UpstreamRuntime, cache_key};
    use crate::upstream::UpstreamRegistry;

    struct FakeDohTransport {
        request: Mutex<Option<DohHttpRequest>>,
        calls: AtomicUsize,
    }

    struct ServFailDohTransport;

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
        assert_eq!(core.cache().store().stats().hits, 2);
    }

    #[tokio::test]
    async fn optimistic_stale_lookup_refreshes_through_late_finalizer() {
        let mut config = Arc::try_unwrap(doh_config()).unwrap();
        config.dns.cache.enabled = true;
        config.dns.cache.optimistic.enabled = true;
        config.dns.cache.failure_ttl = Duration::from_millis(20);
        let config = Arc::new(config);
        let transport = Arc::new(FakeDohTransport::new());
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
                doh_path: None,
                client_id: None,
                client_addr: first_request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&plan, &first_request).unwrap();
        let first = core.resolve(&first_request).await.unwrap();
        assert!(
            matches!(first, CoreOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NoData)
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
            matches!(stale, CoreOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NoData)
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
        config.dns.cache.failure_ttl = Duration::from_millis(20);
        let config = Arc::new(config);
        let old_transport = Arc::new(FakeDohTransport::new());
        let latest_transport = Arc::new(FakeDohTransport::new());
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
                doh_path: None,
                client_id: None,
                client_addr: first_request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&plan, &first_request).unwrap();
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
                doh_path: None,
                client_id: None,
                client_addr: request.context.client.client_addr,
                client_digest: None,
                qname: Some(&qname),
            })
            .unwrap();
        let key = cache_key(&plan, &request).expect("cache must be enabled");
        let response =
            CanonicalResponse::empty_response(&request.query, ResponseCode::NoError).unwrap();
        let sink = core.late_result_sink(&key, &request);
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
    }

    fn config() -> std::sync::Arc<crate::config::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("policy-core");
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 1
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
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

    fn doh_config_with_address(address: &str) -> std::sync::Arc<crate::config::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("policy-doh");
        let source = format!(
            r#"
version: 1
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
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
        ConfigLoader::new(LoadOptions::default().without_snapshot())
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
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 1
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
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

        let answer = core
            .resolve(&request("local.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(answer) = answer else {
            panic!("expected local response");
        };
        assert_eq!(answer.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(answer.ttl().min_ttl, Some(42));

        let nodata = core
            .resolve(&request("local.example.", RecordType::AAAA))
            .await
            .unwrap();
        let CoreOutcome::Response(nodata) = nodata else {
            panic!("expected nodata response");
        };
        assert_eq!(nodata.class(), crate::dns::ResponseClass::NoData);
    }

    #[test]
    fn policy_core_publishes_new_rule_set_snapshot_and_rejects_stale_version() {
        let core = PolicyDnsCore::from_config(rule_config().as_ref(), 42).unwrap();
        let evaluate = |name: &str| {
            let request = request(name, RecordType::A);
            let qname =
                CanonicalDomain::parse(&request.query.question().name().to_ascii()).unwrap();
            core.policy()
                .evaluate(crate::policy::PolicyRequest {
                    listener_id: &ConfigId::new("dns").unwrap(),
                    doh_path: None,
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
        let core = PolicyDnsCore::from_config(config().as_ref(), 42).unwrap();
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

        let CoreOutcome::Response(previous) = core
            .resolve(&request("local.example.", RecordType::A))
            .await
            .unwrap()
        else {
            panic!("expected previous hosts response");
        };
        assert_eq!(previous.class(), crate::dns::ResponseClass::NxDomain);
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
        let core = PolicyDnsCore::from_config(group_config().as_ref(), 42).unwrap();
        assert_eq!(core.host_resource_count(), 1);
        assert_eq!(core.upstream_count(), 3);

        let response = core
            .resolve(&request("group.example.", RecordType::A))
            .await
            .unwrap();
        let CoreOutcome::Response(response) = response else {
            panic!("expected group response");
        };
        assert_eq!(response.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(response.ttl().min_ttl, Some(crate::dns::DEFAULT_LOCAL_TTL));
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
            )
            .await
            .expect("nested group must resolve");
        assert!(matches!(
            outcome,
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
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 1
work:
  path: {work_path}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
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
