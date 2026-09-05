use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use thiserror::Error;

use crate::config::resolve::{ConfigId, ResolvedHostsResource, ResolvedRuleSet};
use crate::config::{BindPlan, ResolvedConfig};
use crate::dns::{Cancellation, DEFAULT_LOCAL_TTL, Deadline, PolicyDnsCore, RuntimeRevision};
use crate::ports::effects::ResourceFetcher;
use crate::resource::{
    FileHostsRefreshWorker, FileRuleSetRefreshWorker, HostsIndex, HostsLimits, RefreshBackoff,
    RemoteResourceOptions, ReqwestResourceFetcher, ResourceRefreshRuntime, ResourceRefreshWorker,
    ResourceRegistrySnapshot, ResourceScheduleDecision, ResourceSchedulePolicy, ResourceSnapshot,
    ResourceSource, ResourceSourceKind, ResourceStaleStatus, RuleIndex, RuleLimits,
    fetch_remote_rule_set, load_hosts, load_rule_set, restore_remote_rule_set,
};

use super::RuntimeSnapshot;

/// 没有对外 socket 的候选运行时。
pub struct PreparedRuntime {
    pub(crate) snapshot: Arc<RuntimeSnapshot>,
    pub(crate) bind_plan: Arc<BindPlan>,
    resource_fetcher: Option<Arc<dyn ResourceFetcher>>,
    resource_snapshots: BTreeMap<ConfigId, ResourceSnapshot<RuleIndex>>,
    host_resource_snapshots: BTreeMap<ConfigId, ResourceSnapshot<HostsIndex>>,
    resource_workers: BTreeMap<ConfigId, PreparedResourceWorker>,
    #[cfg(test)]
    policy_published_gate: Option<(ConfigId, Arc<crate::ports::testing::TestGate>)>,
}

#[derive(Clone)]
enum PreparedResourceWorker {
    RemoteRule(ResourceRefreshWorker),
    FileRule(FileRuleSetRefreshWorker),
    FileHosts(FileHostsRefreshWorker),
}

impl PreparedResourceWorker {
    fn merge_state_from(&mut self, incoming: &Self) {
        match (self, incoming) {
            (Self::RemoteRule(current), Self::RemoteRule(incoming)) => {
                current.merge_state_from(incoming)
            }
            (Self::FileRule(current), Self::FileRule(incoming)) => {
                current.merge_state_from(incoming)
            }
            (Self::FileHosts(current), Self::FileHosts(incoming)) => {
                current.merge_state_from(incoming)
            }
            _ => {}
        }
    }

    fn current_hosts_snapshot(&self) -> Option<ResourceSnapshot<HostsIndex>> {
        match self {
            Self::FileHosts(worker) => worker.current_snapshot(),
            Self::RemoteRule(_) | Self::FileRule(_) => None,
        }
    }

    fn current_rule_snapshot(&self) -> Option<ResourceSnapshot<RuleIndex>> {
        match self {
            Self::RemoteRule(worker) => worker.current_snapshot(),
            Self::FileRule(worker) => worker.current_snapshot(),
            Self::FileHosts(_) => None,
        }
    }
}

type InitialFileSnapshots = (
    BTreeMap<ConfigId, ResourceSnapshot<HostsIndex>>,
    BTreeMap<ConfigId, ResourceSnapshot<RuleIndex>>,
);

#[derive(Clone, Debug)]
pub enum RefreshedResourceSnapshot {
    Hosts(ResourceSnapshot<HostsIndex>),
    RuleSet(ResourceSnapshot<RuleIndex>),
}

impl RefreshedResourceSnapshot {
    pub fn resource_id(&self) -> &ConfigId {
        match self {
            Self::Hosts(snapshot) => snapshot.resource_id(),
            Self::RuleSet(snapshot) => snapshot.resource_id(),
        }
    }

    pub const fn epoch(&self) -> u64 {
        match self {
            Self::Hosts(snapshot) => snapshot.epoch(),
            Self::RuleSet(snapshot) => snapshot.epoch(),
        }
    }

    pub const fn revision(&self) -> u64 {
        match self {
            Self::Hosts(snapshot) => snapshot.revision(),
            Self::RuleSet(snapshot) => snapshot.revision(),
        }
    }
}

impl PreparedRuntime {
    #[cfg(test)]
    pub(crate) fn set_policy_published_gate_for_test(
        &mut self,
        resource: ConfigId,
        gate: Arc<crate::ports::testing::TestGate>,
    ) {
        self.policy_published_gate = Some((resource, gate));
    }

    /// 只接收 Config 阶段已经生成的 immutable `ResolvedConfig`，不重新读取 YAML。
    pub fn prepare(
        config: Arc<ResolvedConfig>,
        revision: RuntimeRevision,
    ) -> Result<Self, PrepareError> {
        let bind_plan = prepare_bind_plan(&config, revision)?;
        let snapshot = Arc::new(RuntimeSnapshot::new(revision, config));
        Ok(Self {
            snapshot,
            bind_plan: Arc::new(bind_plan),
            resource_fetcher: None,
            resource_snapshots: BTreeMap::new(),
            host_resource_snapshots: BTreeMap::new(),
            resource_workers: BTreeMap::new(),
            #[cfg(test)]
            policy_published_gate: None,
        })
    }

    /// 在 socket bind 前完成 Policy/Resource 本地 Core 构建，并把 handle 固定进 snapshot。
    pub fn prepare_with_policy_core(
        config: Arc<ResolvedConfig>,
        revision: RuntimeRevision,
    ) -> Result<Self, PrepareError> {
        let bind_plan = prepare_bind_plan(&config, revision)?;
        let resource_fetcher = ReqwestResourceFetcher::from_resolved(&config.outbounds, 64 * 1024)
            .map_err(|error| PrepareError::ResourceFetcher {
                reason: error.to_string(),
            })?;
        let policy_core =
            PolicyDnsCore::from_config(&config, DEFAULT_LOCAL_TTL).map_err(|error| {
                PrepareError::PolicyCore {
                    reason: error.to_string(),
                }
            })?;
        let snapshot = Arc::new(RuntimeSnapshot::with_policy_core(
            revision,
            config,
            policy_core,
        ));
        Ok(Self {
            snapshot,
            bind_plan: Arc::new(bind_plan),
            resource_fetcher: Some(Arc::new(resource_fetcher)),
            resource_snapshots: BTreeMap::new(),
            host_resource_snapshots: BTreeMap::new(),
            resource_workers: BTreeMap::new(),
            #[cfg(test)]
            policy_published_gate: None,
        })
    }

    /// 在候选运行时对 remote rule-set 完成恢复或首次下载，再构造 Policy core。
    ///
    /// 所有 remote 资源必须在 bind 前得到已编译 snapshot；恢复失败后才尝试网络，
    /// 两者都失败时拒绝候选，不发布半成品 runtime。
    pub async fn prepare_with_policy_core_and_remote_resources(
        config: Arc<ResolvedConfig>,
        revision: RuntimeRevision,
        deadline: Deadline,
        cancellation: Cancellation,
    ) -> Result<Self, PrepareError> {
        let bind_plan = prepare_bind_plan(&config, revision)?;
        let resource_fetcher: Arc<dyn ResourceFetcher> = Arc::new(
            ReqwestResourceFetcher::from_resolved(&config.outbounds, 64 * 1024).map_err(
                |error| PrepareError::ResourceFetcher {
                    reason: error.to_string(),
                },
            )?,
        );
        let (host_snapshots, mut rule_snapshots) = load_initial_file_snapshots(&config)?;
        for resource in &config.rule_sets {
            let crate::config::resolve::ResolvedRuleSet::Remote { id, .. } = resource else {
                continue;
            };
            let options = remote_resource_options(&config, id, deadline, cancellation.clone());
            let loaded = match restore_remote_rule_set(resource, options.clone()) {
                Ok(loaded) => loaded,
                Err(restore_error) => {
                    fetch_remote_rule_set(resource_fetcher.as_ref(), resource, options)
                        .await
                        .map_err(|fetch_error| PrepareError::RemoteResource {
                            resource: id.as_str().to_owned(),
                            reason: format!(
                                "restore failed: {restore_error}; fetch failed: {fetch_error}"
                            ),
                        })?
                }
            };
            rule_snapshots.insert(id.clone(), loaded.snapshot().clone());
        }
        let policy_core = PolicyDnsCore::from_config_with_resource_snapshots(
            &config,
            DEFAULT_LOCAL_TTL,
            &host_snapshots,
            &rule_snapshots,
        )
        .map_err(|error| PrepareError::PolicyCore {
            reason: error.to_string(),
        })?;
        if policy_core.cache().options().enabled {
            match policy_core
                .initialize_cache_persistence(&config, deadline)
                .await
            {
                Ok(summary) => tracing::info!(
                    event = "cache_persistence_ready",
                    component = "cache",
                    result = "success",
                    loaded = summary.loaded,
                    expired = summary.expired,
                    corrupt = summary.corrupt,
                    incompatible = summary.incompatible,
                    "cache_persistence_ready"
                ),
                Err(error) => tracing::warn!(
                    event = "cache_persistence_disabled",
                    component = "cache",
                    result = "degraded",
                    error = %error,
                    "cache_persistence_disabled"
                ),
            }
        }
        let resource_workers = build_resource_workers(
            &config,
            Arc::clone(&resource_fetcher),
            &host_snapshots,
            &rule_snapshots,
        )?;
        let snapshot = Arc::new(RuntimeSnapshot::with_policy_core_and_resources(
            revision,
            config,
            policy_core,
            host_snapshots.values().cloned(),
            rule_snapshots.values().cloned(),
        ));
        Ok(Self {
            snapshot,
            bind_plan: Arc::new(bind_plan),
            resource_fetcher: Some(resource_fetcher),
            resource_snapshots: rule_snapshots,
            host_resource_snapshots: host_snapshots,
            resource_workers,
            #[cfg(test)]
            policy_published_gate: None,
        })
    }

    pub fn snapshot(&self) -> &Arc<RuntimeSnapshot> {
        &self.snapshot
    }

    pub fn bind_plan(&self) -> &BindPlan {
        &self.bind_plan
    }

    pub fn resource_fetcher(&self) -> Option<Arc<dyn ResourceFetcher>> {
        self.resource_fetcher.as_ref().map(Arc::clone)
    }

    pub fn resource_snapshots(&self) -> &BTreeMap<ConfigId, ResourceSnapshot<RuleIndex>> {
        &self.resource_snapshots
    }

    pub fn host_resource_snapshots(&self) -> &BTreeMap<ConfigId, ResourceSnapshot<HostsIndex>> {
        &self.host_resource_snapshots
    }

    /// 合并旧 Runtime 中仍与当前配置完全一致的已发布资源和 worker 状态。
    ///
    /// 候选自身的新配置优先；只有旧 Runtime 的 `ResourceVersion` 严格更高时才
    /// 迁移其 compiled snapshot。worker 只迁移已完成的 schedule/backoff 和 registry，
    /// 不复制 in-flight reservation，避免旧 task 在 reload 后向候选提交结果。
    pub(crate) fn merge_state_from(&mut self, incoming: &Self) {
        let candidate_config = self.snapshot.config();
        let incoming_config = incoming.snapshot.config();

        for (resource_id, incoming_worker) in &incoming.resource_workers {
            if !same_host_resource(candidate_config, incoming_config, resource_id) {
                continue;
            }
            let Some(snapshot) = incoming_worker.current_hosts_snapshot() else {
                continue;
            };
            let should_merge = self
                .host_resource_snapshots
                .get(resource_id)
                .is_none_or(|current| snapshot.version() > current.version());
            if should_merge {
                self.host_resource_snapshots
                    .insert(resource_id.clone(), snapshot.clone());
                if let Some(policy) = self.snapshot.policy_core()
                    && let Err(error) = policy.publish_hosts_resource(snapshot)
                {
                    debug_assert!(matches!(
                        error,
                        crate::dns::PolicyResourcePublishError::StaleVersion { .. }
                    ));
                }
            }
        }

        for (resource_id, incoming_worker) in &incoming.resource_workers {
            if !same_rule_set(candidate_config, incoming_config, resource_id) {
                continue;
            }
            let Some(snapshot) = incoming_worker.current_rule_snapshot() else {
                continue;
            };
            let should_merge = self
                .resource_snapshots
                .get(resource_id)
                .is_none_or(|current| snapshot.version() > current.version());
            if should_merge {
                self.resource_snapshots
                    .insert(resource_id.clone(), snapshot.clone());
                if let Some(policy) = self.snapshot.policy_core()
                    && let Err(error) = policy.publish_rule_set_resource(snapshot)
                {
                    debug_assert!(matches!(
                        error,
                        crate::dns::PolicyResourcePublishError::StaleVersion { .. }
                    ));
                }
            }
        }

        for (resource_id, snapshot) in &incoming.host_resource_snapshots {
            if !same_host_resource(candidate_config, incoming_config, resource_id) {
                continue;
            }
            let should_merge = self
                .host_resource_snapshots
                .get(resource_id)
                .is_none_or(|current| snapshot.version() > current.version());
            if !should_merge {
                continue;
            }
            self.host_resource_snapshots
                .insert(resource_id.clone(), snapshot.clone());
            if let Some(policy) = self.snapshot.policy_core()
                && let Err(error) = policy.publish_hosts_resource(snapshot.clone())
            {
                debug_assert!(
                    matches!(
                        error,
                        crate::dns::PolicyResourcePublishError::StaleVersion { .. }
                    ),
                    "candidate policy must register every compatible host resource"
                );
            }
        }

        for (resource_id, snapshot) in &incoming.resource_snapshots {
            if !same_rule_set(candidate_config, incoming_config, resource_id) {
                continue;
            }
            let should_merge = self
                .resource_snapshots
                .get(resource_id)
                .is_none_or(|current| snapshot.version() > current.version());
            if !should_merge {
                continue;
            }
            self.resource_snapshots
                .insert(resource_id.clone(), snapshot.clone());
            if let Some(policy) = self.snapshot.policy_core()
                && let Err(error) = policy.publish_rule_set_resource(snapshot.clone())
            {
                debug_assert!(
                    matches!(
                        error,
                        crate::dns::PolicyResourcePublishError::StaleVersion { .. }
                    ),
                    "candidate policy must register every compatible rule-set resource"
                );
            }
        }

        self.snapshot
            .merge_resource_metadata_from(&incoming.snapshot, |resource_id| {
                same_resource(candidate_config, incoming_config, resource_id)
            });

        for (resource_id, worker) in &mut self.resource_workers {
            if !same_resource(candidate_config, incoming_config, resource_id) {
                continue;
            }
            let Some(incoming_worker) = incoming.resource_workers.get(resource_id) else {
                continue;
            };
            worker.merge_state_from(incoming_worker);
        }
    }

    pub fn resource_worker_ids(&self) -> Vec<ConfigId> {
        self.resource_workers.keys().cloned().collect()
    }

    pub fn resource_refresh_decision(
        &self,
        resource: &ConfigId,
        now: u64,
    ) -> Option<ResourceScheduleDecision> {
        self.resource_workers
            .get(resource)
            .and_then(|worker| match worker {
                PreparedResourceWorker::RemoteRule(worker) => {
                    worker.runtime().decision(resource, now)
                }
                PreparedResourceWorker::FileRule(worker) => {
                    worker.runtime().decision(resource, now)
                }
                PreparedResourceWorker::FileHosts(worker) => {
                    worker.runtime().decision(resource, now)
                }
            })
    }

    pub async fn refresh_resource(
        &self,
        resource: &ConfigId,
        now: u64,
        deadline: Deadline,
        cancellation: Cancellation,
    ) -> Result<RefreshedResourceSnapshot, ResourceRefreshError> {
        let worker = self
            .resource_workers
            .get(resource)
            .cloned()
            .ok_or_else(|| ResourceRefreshError::NotConfigured {
                resource: resource.as_str().to_owned(),
            })?;
        let refreshed = match worker {
            PreparedResourceWorker::RemoteRule(worker) => {
                let rule_set = self
                    .snapshot
                    .config()
                    .rule_sets
                    .iter()
                    .find(|candidate| resolved_rule_set_id(candidate) == resource)
                    .ok_or_else(|| ResourceRefreshError::NotConfigured {
                        resource: resource.as_str().to_owned(),
                    })?;
                let options = remote_resource_options(
                    self.snapshot.config_arc().as_ref(),
                    resource,
                    deadline,
                    cancellation,
                );
                let loaded = worker
                    .refresh_remote_rule_set(rule_set, options, now)
                    .await
                    .map_err(|error| ResourceRefreshError::Worker {
                        resource: resource.as_str().to_owned(),
                        reason: error.to_string(),
                    })?;
                RefreshedResourceSnapshot::RuleSet(loaded.snapshot().clone())
            }
            PreparedResourceWorker::FileRule(worker) => worker
                .refresh(now)
                .map(RefreshedResourceSnapshot::RuleSet)
                .map_err(|error| ResourceRefreshError::Worker {
                    resource: resource.as_str().to_owned(),
                    reason: error.to_string(),
                })?,
            PreparedResourceWorker::FileHosts(worker) => worker
                .refresh(now)
                .map(RefreshedResourceSnapshot::Hosts)
                .map_err(|error| ResourceRefreshError::Worker {
                    resource: resource.as_str().to_owned(),
                    reason: error.to_string(),
                })?,
        };
        let policy = self
            .snapshot
            .policy_core()
            .ok_or(ResourceRefreshError::MissingPolicyCore)?;
        match &refreshed {
            RefreshedResourceSnapshot::Hosts(snapshot) => policy
                .publish_hosts_resource(snapshot.clone())
                .map_err(|error| ResourceRefreshError::Policy {
                    resource: resource.as_str().to_owned(),
                    reason: error.to_string(),
                })?,
            RefreshedResourceSnapshot::RuleSet(snapshot) => policy
                .publish_rule_set_resource(snapshot.clone())
                .map_err(|error| ResourceRefreshError::Policy {
                    resource: resource.as_str().to_owned(),
                    reason: error.to_string(),
                })?,
        }
        // 仅测试可停在两个发布点之间；正式构建没有额外等待或配置。
        #[cfg(test)]
        if let Some((target, gate)) = &self.policy_published_gate
            && target == resource
        {
            gate.pause().await;
        }
        let metadata_result = match &refreshed {
            RefreshedResourceSnapshot::Hosts(snapshot) => self.snapshot.publish_resource(snapshot),
            RefreshedResourceSnapshot::RuleSet(snapshot) => {
                self.snapshot.publish_resource(snapshot)
            }
        };
        metadata_result.map_err(|error| ResourceRefreshError::Snapshot {
            resource: resource.as_str().to_owned(),
            reason: format!("{error:?}"),
        })?;
        Ok(refreshed)
    }

    pub async fn refresh_remote_rule_set(
        &self,
        resource: &ConfigId,
        now: u64,
        deadline: Deadline,
        cancellation: Cancellation,
    ) -> Result<ResourceSnapshot<RuleIndex>, ResourceRefreshError> {
        if !self
            .snapshot
            .config()
            .rule_sets
            .iter()
            .any(|candidate| resolved_rule_set_id(candidate) == resource)
        {
            return Err(ResourceRefreshError::NotConfigured {
                resource: resource.as_str().to_owned(),
            });
        }
        match self
            .refresh_resource(resource, now, deadline, cancellation)
            .await?
        {
            RefreshedResourceSnapshot::RuleSet(snapshot) => Ok(snapshot),
            RefreshedResourceSnapshot::Hosts(_) => Err(ResourceRefreshError::NotConfigured {
                resource: resource.as_str().to_owned(),
            }),
        }
    }

    pub fn shutdown_resource_refresh(&self) {
        for worker in self.resource_workers.values() {
            match worker {
                PreparedResourceWorker::RemoteRule(worker) => worker.runtime().shutdown(),
                PreparedResourceWorker::FileRule(worker) => worker.runtime().shutdown(),
                PreparedResourceWorker::FileHosts(worker) => worker.runtime().shutdown(),
            }
        }
    }

    pub fn preflight(&self) -> PreflightReport {
        PreflightReport {
            revision: self.snapshot.revision(),
            endpoint_count: self.bind_plan.entries.len(),
            normalized_hash: self.snapshot.config().normalized_hash.clone(),
            has_policy_core: self.snapshot.policy_core().is_some(),
            has_resource_fetcher: self.resource_fetcher.is_some(),
            resource_snapshot_count: self.resource_snapshots.len()
                + self.host_resource_snapshots.len(),
            resource_worker_count: self.resource_workers.len(),
        }
    }
}

impl fmt::Debug for PreparedRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedRuntime")
            .field("snapshot", &self.snapshot)
            .field("bind_entry_count", &self.bind_plan.entries.len())
            .field("has_resource_fetcher", &self.resource_fetcher.is_some())
            .field(
                "resource_snapshot_count",
                &(self.resource_snapshots.len() + self.host_resource_snapshots.len()),
            )
            .field("resource_worker_count", &self.resource_workers.len())
            .finish()
    }
}

/// prepare 阶段的稳定错误；不携带原始配置正文或秘密值。
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PrepareError {
    #[error("runtime revision must be greater than zero")]
    InvalidRevision,
    #[error("runtime bind plan must contain at least one endpoint")]
    EmptyBindPlan,
    #[error("runtime bind entry {index} has an invalid port")]
    InvalidPort { index: usize },
    #[error("runtime bind entry {index} has an empty owner")]
    EmptyOwner { index: usize },
    #[error("runtime bind entry {index} duplicates another endpoint")]
    DuplicateEndpoint { index: usize },
    #[error("runtime policy DNS core could not be built: {reason}")]
    PolicyCore { reason: String },
    #[error("runtime resource fetcher could not be built: {reason}")]
    ResourceFetcher { reason: String },
    #[error("remote resource `{resource}` could not be prepared: {reason}")]
    RemoteResource { resource: String, reason: String },
    #[error("resource `{resource}` refresh worker could not be prepared: {reason}")]
    ResourceRefresh { resource: String, reason: String },
    #[error("file hosts resource `{resource}` could not be prepared: {reason}")]
    FileHostsResource { resource: String, reason: String },
    #[error("file rule-set resource `{resource}` could not be prepared: {reason}")]
    FileRuleSetResource { resource: String, reason: String },
}

#[derive(Debug, Error)]
pub enum ResourceRefreshError {
    #[error("runtime resource `{resource}` has no configured refresh worker")]
    NotConfigured { resource: String },
    #[error("runtime resource refresh worker for `{resource}` failed: {reason}")]
    Worker { resource: String, reason: String },
    #[error("runtime policy DNS core is unavailable for resource refresh")]
    MissingPolicyCore,
    #[error("runtime resource `{resource}` could not be published to policy: {reason}")]
    Policy { resource: String, reason: String },
    #[error("runtime resource `{resource}` metadata could not be published: {reason}")]
    Snapshot { resource: String, reason: String },
}

fn same_resource(
    candidate: &ResolvedConfig,
    incoming: &ResolvedConfig,
    resource_id: &ConfigId,
) -> bool {
    let candidate_host = find_host_resource(candidate, resource_id);
    let incoming_host = find_host_resource(incoming, resource_id);
    let candidate_rule = find_rule_set(candidate, resource_id);
    let incoming_rule = find_rule_set(incoming, resource_id);
    match (candidate_host, incoming_host, candidate_rule, incoming_rule) {
        (Some(left), Some(right), None, None) => left == right,
        (None, None, Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn same_host_resource(
    candidate: &ResolvedConfig,
    incoming: &ResolvedConfig,
    resource_id: &ConfigId,
) -> bool {
    find_host_resource(candidate, resource_id) == find_host_resource(incoming, resource_id)
}

fn same_rule_set(
    candidate: &ResolvedConfig,
    incoming: &ResolvedConfig,
    resource_id: &ConfigId,
) -> bool {
    find_rule_set(candidate, resource_id) == find_rule_set(incoming, resource_id)
}

fn find_host_resource<'a>(
    config: &'a ResolvedConfig,
    resource_id: &ConfigId,
) -> Option<&'a ResolvedHostsResource> {
    config.hosts.iter().find(|resource| match resource {
        ResolvedHostsResource::Const { id, .. } | ResolvedHostsResource::File { id, .. } => {
            id == resource_id
        }
    })
}

fn find_rule_set<'a>(
    config: &'a ResolvedConfig,
    resource_id: &ConfigId,
) -> Option<&'a ResolvedRuleSet> {
    config.rule_sets.iter().find(|resource| match resource {
        ResolvedRuleSet::Const { id, .. }
        | ResolvedRuleSet::File { id, .. }
        | ResolvedRuleSet::Remote { id, .. } => id == resource_id,
    })
}

/// prepare 成功后可用于观测和验收的最小摘要。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightReport {
    pub revision: RuntimeRevision,
    pub endpoint_count: usize,
    pub normalized_hash: String,
    pub has_policy_core: bool,
    pub has_resource_fetcher: bool,
    pub resource_snapshot_count: usize,
    pub resource_worker_count: usize,
}

fn build_resource_workers(
    config: &ResolvedConfig,
    fetcher: Arc<dyn ResourceFetcher>,
    host_snapshots: &BTreeMap<ConfigId, ResourceSnapshot<HostsIndex>>,
    rule_snapshots: &BTreeMap<ConfigId, ResourceSnapshot<RuleIndex>>,
) -> Result<BTreeMap<ConfigId, PreparedResourceWorker>, PrepareError> {
    let initial_due = unix_seconds();
    let mut workers = BTreeMap::new();
    let refresh_policy = |id: &ConfigId, interval: Option<std::time::Duration>| {
        let interval = interval.map_or(0, |value| value.as_secs().max(1));
        if interval == 0 {
            return Err(PrepareError::ResourceRefresh {
                resource: id.as_str().to_owned(),
                reason: "auto_update requires a positive update interval".to_owned(),
            });
        }
        Ok((
            interval,
            ResourceSchedulePolicy::new(
                interval,
                3,
                3,
                RefreshBackoff::new(1, 300).expect("static refresh backoff is valid"),
            )
            .expect("validated refresh policy is non-zero"),
        ))
    };

    for resource in &config.hosts {
        let ResolvedHostsResource::File {
            id,
            auto_update,
            update_interval,
            ..
        } = resource
        else {
            continue;
        };
        if !auto_update {
            continue;
        }
        let (interval, policy) = refresh_policy(id, *update_interval)?;
        let snapshot = host_snapshots
            .get(id)
            .ok_or_else(|| PrepareError::FileHostsResource {
                resource: id.as_str().to_owned(),
                reason: "compiled file hosts snapshot is missing".to_owned(),
            })?;
        let registry = ResourceRegistrySnapshot::new()
            .publish(snapshot.clone())
            .map_err(|error| PrepareError::FileHostsResource {
                resource: id.as_str().to_owned(),
                reason: format!("refresh registry could not be initialized: {error:?}"),
            })?;
        let runtime =
            ResourceRefreshRuntime::new(registry, policy, initial_due.saturating_add(interval));
        workers.insert(
            id.clone(),
            PreparedResourceWorker::FileHosts(FileHostsRefreshWorker::new(
                runtime,
                resource.clone(),
            )),
        );
    }

    for resource in &config.rule_sets {
        let (id, auto_update, update_interval) = match resource {
            ResolvedRuleSet::File {
                id,
                auto_update,
                update_interval,
                ..
            }
            | ResolvedRuleSet::Remote {
                id,
                auto_update,
                update_interval,
                ..
            } => (id, *auto_update, *update_interval),
            ResolvedRuleSet::Const { .. } => continue,
        };
        if !auto_update {
            continue;
        }
        let (interval, policy) = refresh_policy(id, update_interval)?;
        let snapshot = rule_snapshots
            .get(id)
            .ok_or_else(|| PrepareError::FileRuleSetResource {
                resource: id.as_str().to_owned(),
                reason: "compiled file or remote rule-set snapshot is missing".to_owned(),
            })?;
        let registry = ResourceRegistrySnapshot::new()
            .publish(snapshot.clone())
            .map_err(|error| PrepareError::FileRuleSetResource {
                resource: id.as_str().to_owned(),
                reason: format!("refresh registry could not be initialized: {error:?}"),
            })?;
        let runtime =
            ResourceRefreshRuntime::new(registry, policy, initial_due.saturating_add(interval));
        let worker = match resource {
            ResolvedRuleSet::File { .. } => PreparedResourceWorker::FileRule(
                FileRuleSetRefreshWorker::new(runtime, resource.clone()),
            ),
            ResolvedRuleSet::Remote { .. } => PreparedResourceWorker::RemoteRule(
                ResourceRefreshWorker::new(runtime, Arc::clone(&fetcher)),
            ),
            ResolvedRuleSet::Const { .. } => unreachable!("const rule-set was skipped"),
        };
        workers.insert(id.clone(), worker);
    }
    Ok(workers)
}

fn load_initial_file_snapshots(
    config: &ResolvedConfig,
) -> Result<InitialFileSnapshots, PrepareError> {
    let mut hosts = BTreeMap::new();
    for resource in &config.hosts {
        let ResolvedHostsResource::File { id, .. } = resource else {
            continue;
        };
        let loaded = load_hosts(resource, HostsLimits::default()).map_err(|error| {
            PrepareError::FileHostsResource {
                resource: id.as_str().to_owned(),
                reason: error.to_string(),
            }
        })?;
        hosts.insert(id.clone(), local_hosts_snapshot(&loaded));
    }

    let mut rule_sets = BTreeMap::new();
    for resource in &config.rule_sets {
        let ResolvedRuleSet::File { id, .. } = resource else {
            continue;
        };
        let loaded = load_rule_set(resource, RuleLimits::default()).map_err(|error| {
            PrepareError::FileRuleSetResource {
                resource: id.as_str().to_owned(),
                reason: error.to_string(),
            }
        })?;
        rule_sets.insert(id.clone(), local_rule_snapshot(&loaded));
    }
    Ok((hosts, rule_sets))
}

fn local_hosts_snapshot(
    loaded: &crate::resource::LoadedHostsResource,
) -> ResourceSnapshot<HostsIndex> {
    ResourceSnapshot::new(
        loaded.id().clone(),
        1,
        1,
        loaded.content_hash().to_owned(),
        local_source_fingerprint(loaded.source()),
        loaded.parser_version(),
        loaded.fetched_at(),
        ResourceSourceKind::File,
        false,
        ResourceStaleStatus::Fresh,
        loaded.index().clone(),
    )
}

fn local_rule_snapshot(
    loaded: &crate::resource::LoadedRuleSetResource,
) -> ResourceSnapshot<RuleIndex> {
    ResourceSnapshot::new(
        loaded.id().clone(),
        1,
        1,
        loaded.content_hash().to_owned(),
        local_source_fingerprint(loaded.source()),
        loaded.parser_version(),
        loaded.fetched_at(),
        ResourceSourceKind::File,
        false,
        ResourceStaleStatus::Fresh,
        loaded.index().clone(),
    )
}

fn local_source_fingerprint(source: &ResourceSource) -> String {
    match source {
        ResourceSource::Const => "const".to_owned(),
        ResourceSource::File { fingerprint, .. } => format!(
            "file:{}:{}",
            fingerprint.byte_len(),
            fingerprint.modified_unix_nanos().unwrap_or_default()
        ),
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn resolved_rule_set_id(
    resource: &crate::config::resolve::ResolvedRuleSet,
) -> &crate::config::resolve::ConfigId {
    match resource {
        crate::config::resolve::ResolvedRuleSet::Const { id, .. }
        | crate::config::resolve::ResolvedRuleSet::File { id, .. }
        | crate::config::resolve::ResolvedRuleSet::Remote { id, .. } => id,
    }
}

fn remote_resource_options(
    config: &ResolvedConfig,
    resource: &crate::config::resolve::ConfigId,
    deadline: Deadline,
    cancellation: Cancellation,
) -> RemoteResourceOptions {
    let content_path = config
        .work
        .rules_path
        .join(format!("{}.content", resource.as_str()));
    let manifest_path = config
        .work
        .rules_path
        .join(format!("{}.manifest", resource.as_str()));
    RemoteResourceOptions::new(
        crate::resource::RuleLimits::default().max_input_bytes,
        content_path,
        manifest_path,
        deadline,
        cancellation,
    )
}

fn prepare_bind_plan(
    config: &ResolvedConfig,
    revision: RuntimeRevision,
) -> Result<BindPlan, PrepareError> {
    if revision.0 == 0 {
        return Err(PrepareError::InvalidRevision);
    }
    validate_bind_plan(&config.bind_plan)
}

fn validate_bind_plan(plan: &BindPlan) -> Result<BindPlan, PrepareError> {
    if plan.entries.is_empty() {
        return Err(PrepareError::EmptyBindPlan);
    }

    let mut seen = BTreeSet::<(crate::config::BindProtocol, IpAddr, u16)>::new();
    for (index, entry) in plan.entries.iter().enumerate() {
        if entry.port == 0 {
            return Err(PrepareError::InvalidPort { index });
        }
        if entry.owner.trim().is_empty() {
            return Err(PrepareError::EmptyOwner { index });
        }
        if !seen.insert((entry.protocol, entry.address, entry.port)) {
            return Err(PrepareError::DuplicateEndpoint { index });
        }
    }

    Ok(plan.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::config::resolve::{ConfigId, ResolvedHostsResource, ResolvedUpstream};
    use crate::config::{BindPlan, ConfigLoader, LoadOptions};
    use crate::dns::{Cancellation, Deadline, RuntimeRevision};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{PrepareError, PreparedRuntime};

    fn config() -> Arc<crate::config::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("runtime");
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
    hosts: "127.0.0.1 example.test"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: "127.0.0.1 example.test"
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
"#,
            ))
            .expect("runtime fixture must be valid")
            .resolved
    }

    fn remote_config(port: u16, work: &std::path::Path) -> Arc<crate::config::ResolvedConfig> {
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 1
work:
  path: {}
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
    hosts: "127.0.0.1 fallback.test"
hosts: []
outbound: []
rule_set:
  - type: remote
    name: remote-rules
    format: clash
    url: http://127.0.0.1:{port}/rules
    auto_update: true
    update_interval: 1s
strategy:
  - name: default
    rules:
      - rule_set: remote-rules
        upstream: local
    default_upstream: local
clients: []
"#,
                work.display()
            ))
            .expect("remote runtime fixture must be valid")
            .resolved
    }

    fn dat_fixture() -> Vec<u8> {
        let mut bytes = vec![
            0x0a, 0x16, 0x0a, 0x02, b'C', b'N', 0x12, 0x10, 0x08, 0x03, 0x12, 0x0c,
        ];
        bytes.extend_from_slice(b"example.test");
        bytes
    }

    #[test]
    fn prepare_creates_a_candidate_without_binding() {
        let config = config();
        let candidate = PreparedRuntime::prepare(Arc::clone(&config), RuntimeRevision(1)).unwrap();

        assert_eq!(candidate.preflight().endpoint_count, 1);
        assert_eq!(candidate.snapshot().revision(), RuntimeRevision(1));
        assert_eq!(candidate.bind_plan().entries.len(), 1);
        assert!(Arc::ptr_eq(&candidate.snapshot().config_arc(), &config));
        assert!(!candidate.preflight().has_policy_core);
        assert!(!candidate.preflight().has_resource_fetcher);
        assert!(candidate.resource_fetcher().is_none());
        assert!(candidate.snapshot().dns_core().is_none());
    }

    #[test]
    fn prepare_with_policy_core_captures_one_immutable_core() {
        let candidate =
            PreparedRuntime::prepare_with_policy_core(config(), RuntimeRevision(2)).unwrap();

        assert!(candidate.preflight().has_policy_core);
        assert!(candidate.preflight().has_resource_fetcher);
        assert!(candidate.resource_fetcher().is_some());
        assert!(candidate.snapshot().policy_core().is_some());
        assert!(candidate.snapshot().dns_core().is_some());
    }

    #[tokio::test]
    async fn async_prepare_attaches_cache_persistence_before_bind() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-runtime-cache-persistence-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = Arc::try_unwrap(config()).unwrap();
        config.dns.cache.enabled = true;
        config.dns.cache.persistence_path = root.join("cache.sqlite3");
        config.dns.cache.persistence_max_size_bytes = 1024 * 1024;
        let deadline = Deadline::new(Instant::now() + Duration::from_secs(5));

        let candidate = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            Arc::new(config),
            RuntimeRevision(3),
            deadline,
            Cancellation::new(),
        )
        .await
        .unwrap();
        let core = candidate.snapshot().policy_core().unwrap();
        assert!(core.has_cache_persistence());
        assert!(
            core.finalizer_owner()
                .shutdown_until(deadline)
                .await
                .completed
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn async_prepare_restores_or_fetches_remote_rule_set_before_bind() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for body in [
                b"DOMAIN-SUFFIX,example.test\n".as_slice(),
                b"DOMAIN-SUFFIX,updated.test\n".as_slice(),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 1024];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let root = std::env::temp_dir().join(format!(
            "fluxdns-runtime-remote-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = remote_config(address.port(), &root);
        let candidate = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            Arc::clone(&config),
            RuntimeRevision(4),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        assert_eq!(candidate.preflight().resource_snapshot_count, 1);
        assert_eq!(candidate.preflight().resource_worker_count, 1);
        assert_eq!(candidate.resource_snapshots().len(), 1);
        assert_eq!(
            candidate
                .resource_snapshots()
                .get(&ConfigId::new("remote-rules").unwrap())
                .unwrap()
                .compiled()
                .suffix_count(),
            1
        );
        assert!(root.join("rules/remote-rules.content").is_file());
        assert!(root.join("rules/remote-rules.manifest").is_file());
        let resource = ConfigId::new("remote-rules").unwrap();
        assert!(
            candidate
                .resource_refresh_decision(&resource, u64::MAX)
                .unwrap()
                .is_due()
        );
        let refreshed = candidate
            .refresh_remote_rule_set(
                &resource,
                u64::MAX,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(refreshed.epoch(), 1);
        assert_eq!(refreshed.compiled().suffix_count(), 1);
        task.await.unwrap();

        let restored = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            config,
            RuntimeRevision(5),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        assert!(
            restored.resource_snapshots()[&ConfigId::new("remote-rules").unwrap()].used_fallback()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn async_prepare_builds_file_worker_and_publishes_runtime_metadata() {
        let path = std::env::temp_dir().join(format!(
            "fluxdns-runtime-file-{}-{}.hosts",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "192.0.2.10 old.example\n").unwrap();

        let mut config = config();
        Arc::get_mut(&mut config).unwrap().hosts = vec![ResolvedHostsResource::File {
            id: ConfigId::new("local-hosts").unwrap(),
            format: crate::config::model::HostsFormat::Hosts,
            path: path.clone(),
            auto_update: true,
            update_interval: Some(Duration::from_secs(1)),
        }];
        let candidate = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            Arc::clone(&config),
            RuntimeRevision(6),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(candidate.preflight().resource_snapshot_count, 1);
        assert_eq!(candidate.preflight().resource_worker_count, 1);
        assert_eq!(candidate.host_resource_snapshots().len(), 1);

        std::fs::write(&path, "192.0.2.11 new.example\n").unwrap();
        let refreshed = candidate
            .refresh_resource(
                &ConfigId::new("local-hosts").unwrap(),
                u64::MAX,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(refreshed.epoch(), 2);
        assert_eq!(
            candidate
                .snapshot()
                .resources()
                .lookup(&ConfigId::new("local-hosts").unwrap())
                .unwrap()
                .version(),
            crate::resource::ResourceVersion::new(2, 0)
        );

        std::fs::remove_file(&path).unwrap();
        let error = candidate
            .refresh_resource(
                &ConfigId::new("local-hosts").unwrap(),
                u64::MAX,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, super::ResourceRefreshError::Worker { .. }));
        assert_eq!(
            candidate
                .resource_refresh_decision(&ConfigId::new("local-hosts").unwrap(), u64::MAX)
                .unwrap()
                .consecutive_failures(),
            1
        );
    }

    #[tokio::test]
    async fn async_prepare_compiles_file_dat_selector_before_bind() {
        let path = std::env::temp_dir().join(format!(
            "fluxdns-runtime-dat-{}-{}.dat",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, dat_fixture()).unwrap();
        let resource_id = ConfigId::new("geosite").unwrap();
        let mut config = Arc::try_unwrap(config()).unwrap();
        config.rule_sets = vec![crate::config::resolve::ResolvedRuleSet::File {
            id: resource_id.clone(),
            format: crate::config::model::RuleSetFormat::Dat,
            path: path.clone(),
            auto_update: false,
            update_interval: None,
        }];
        let strategy_ecs = config.strategies[0].edns_client_subnet.clone();
        config.strategies[0].rules = vec![crate::config::resolve::ResolvedStrategyRule {
            rule_set: Some(crate::config::resolve::ResolvedRuleSetRef {
                resource: resource_id.clone(),
                selector: Some("cn".to_owned()),
            }),
            hosts: None,
            upstream: Some(ConfigId::new("local").unwrap()),
            edns_client_subnet: strategy_ecs,
        }];

        let candidate = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            Arc::new(config),
            RuntimeRevision(7),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(candidate.preflight().resource_snapshot_count, 1);
        assert!(
            candidate.resource_snapshots()[&resource_id]
                .compiled()
                .selector("cn")
                .is_some()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn merge_state_preserves_newer_resource_policy_metadata_and_schedule() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-runtime-merge-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("hosts.txt");
        std::fs::write(&path, "192.0.2.10 old.example\n").unwrap();

        let file_config = |path: &std::path::Path| {
            let mut config = Arc::try_unwrap(config()).expect("runtime fixture must be unique");
            config.hosts = vec![ResolvedHostsResource::File {
                id: ConfigId::new("local-hosts").unwrap(),
                format: crate::config::model::HostsFormat::Hosts,
                path: path.to_owned(),
                auto_update: true,
                update_interval: Some(Duration::from_secs(1)),
            }];
            Arc::new(config)
        };

        let active = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            file_config(&path),
            RuntimeRevision(1),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        let mut candidate = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            file_config(&path),
            RuntimeRevision(2),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();

        std::fs::write(&path, "192.0.2.11 new.example\n").unwrap();
        active
            .refresh_resource(
                &ConfigId::new("local-hosts").unwrap(),
                u64::MAX,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();

        candidate.merge_state_from(&active);
        let resource = ConfigId::new("local-hosts").unwrap();
        let merged = candidate.host_resource_snapshots().get(&resource).unwrap();
        assert_eq!(merged.epoch(), 2);
        assert!(
            merged
                .compiled()
                .lookup(&crate::resource::CanonicalDomain::parse("new.example").unwrap())
                .is_some()
        );
        assert_eq!(
            candidate
                .snapshot()
                .resources()
                .lookup(&resource)
                .unwrap()
                .version(),
            crate::resource::ResourceVersion::new(2, 0)
        );
        let policy_index = candidate.snapshot().policy_core().unwrap().policy();
        assert!(
            policy_index
                .hosts_index(&resource)
                .unwrap()
                .lookup(&crate::resource::CanonicalDomain::parse("new.example").unwrap())
                .is_some()
        );
        assert_eq!(
            candidate
                .resource_refresh_decision(&resource, u64::MAX - 1)
                .unwrap()
                .next_due(),
            Some(u64::MAX)
        );

        let _ = std::fs::remove_dir_all(root);
    }

    // V1-P01：Policy 已发布、metadata 未发布时叠加另一资源和 candidate 换代。
    #[tokio::test]
    async fn contract_v1_split_publication_keeps_resources_and_generations_isolated() {
        use crate::ports::testing::TestGate;
        use crate::resource::{CanonicalDomain, ResourceVersion};
        use crate::runtime::RuntimeCoordinator;

        let root = std::env::temp_dir().join(format!(
            "fluxdns-publication-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&root).unwrap();
        let first = ConfigId::new("local-hosts").unwrap();
        let second = ConfigId::new("other-hosts").unwrap();
        let mut source = Arc::try_unwrap(config()).unwrap();
        source.hosts = [first.clone(), second.clone()]
            .into_iter()
            .map(|id| {
                let path = root.join(format!("{}.hosts", id.as_str()));
                std::fs::write(&path, "192.0.2.10 old.example\n").unwrap();
                ResolvedHostsResource::File {
                    id,
                    format: crate::config::model::HostsFormat::Hosts,
                    path,
                    auto_update: true,
                    update_interval: Some(Duration::from_secs(1)),
                }
            })
            .collect();
        let source = Arc::new(source);
        let budget = Deadline::new(Instant::now() + Duration::from_secs(5));
        let mut old = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            Arc::clone(&source),
            RuntimeRevision(1),
            budget,
            Cancellation::new(),
        )
        .await
        .unwrap();
        let candidate = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            source,
            RuntimeRevision(2),
            budget,
            Cancellation::new(),
        )
        .await
        .unwrap();
        let gate = Arc::new(TestGate::new());
        old.policy_published_gate = Some((first.clone(), Arc::clone(&gate)));
        let coordinator = RuntimeCoordinator::new(super::super::bind::test_candidate(old));
        let previous = coordinator.load();
        let refresh_runtime = Arc::clone(&previous);
        let refresh_id = first.clone();
        std::fs::write(
            root.join("local-hosts.hosts"),
            "192.0.2.11 first-new.example\n",
        )
        .unwrap();
        let refresh = tokio::spawn(async move {
            refresh_runtime
                .refresh_resource(&refresh_id, u64::MAX, budget, Cancellation::new())
                .await
        });
        gate.wait_reached().await;
        let first_name = CanonicalDomain::parse("first-new.example").unwrap();
        assert!(
            previous
                .snapshot()
                .policy_core()
                .unwrap()
                .policy()
                .hosts_index(&first)
                .unwrap()
                .lookup(&first_name)
                .is_some()
        );
        assert_eq!(
            previous
                .snapshot()
                .resources()
                .lookup(&first)
                .unwrap()
                .version(),
            ResourceVersion::new(1, 1)
        );

        std::fs::write(
            root.join("other-hosts.hosts"),
            "192.0.2.12 second-new.example\n",
        )
        .unwrap();
        previous
            .refresh_resource(&second, u64::MAX, budget, Cancellation::new())
            .await
            .unwrap();
        assert_eq!(
            previous
                .snapshot()
                .resources()
                .lookup(&second)
                .unwrap()
                .version(),
            ResourceVersion::new(2, 0)
        );
        coordinator.activate(super::super::bind::test_candidate(candidate));
        gate.release();
        tokio::time::timeout(Duration::from_secs(5), refresh)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // 迟到的旧 metadata 只更新旧 snapshot，不能覆盖新实例。
        assert_eq!(
            previous
                .snapshot()
                .resources()
                .lookup(&first)
                .unwrap()
                .version(),
            ResourceVersion::new(2, 0)
        );
        assert_eq!(
            previous
                .snapshot()
                .resources()
                .lookup(&second)
                .unwrap()
                .version(),
            ResourceVersion::new(2, 0)
        );
        let current = coordinator.load();
        assert_eq!(current.snapshot().revision(), RuntimeRevision(2));
        assert_eq!(
            current
                .snapshot()
                .resources()
                .lookup(&first)
                .unwrap()
                .version(),
            ResourceVersion::new(1, 1)
        );
        assert!(
            current
                .snapshot()
                .policy_core()
                .unwrap()
                .policy()
                .hosts_index(&first)
                .unwrap()
                .lookup(&first_name)
                .is_none()
        );
        assert!(previous.is_draining());
        assert_eq!(previous.active_requests(), 0);
        coordinator.begin_drain();
        assert!(coordinator.wait_for_drain(budget).await);
        assert!(coordinator.shutdown_finalizers(budget).await.completed);
        drop(previous);
        drop(current);
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepare_with_policy_core_propagates_missing_proxy_profile() {
        let mut config = Arc::try_unwrap(config()).ok().unwrap();
        config.upstreams.push(ResolvedUpstream::Doh {
            id: ConfigId::new("remote").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: Some("192.0.2.44".parse().unwrap()),
            proxy: Some(ConfigId::new("missing-proxy").unwrap()),
            edns_client_subnet: None,
        });

        let error = PreparedRuntime::prepare_with_policy_core(Arc::new(config), RuntimeRevision(3))
            .unwrap_err();
        let PrepareError::PolicyCore { reason } = error else {
            panic!("expected policy core preparation error");
        };
        assert!(reason.contains("missing outbound `missing-proxy`"));
    }

    #[test]
    fn invalid_revision_is_rejected_before_candidate_creation() {
        assert_eq!(
            PreparedRuntime::prepare(config(), RuntimeRevision(0)).unwrap_err(),
            PrepareError::InvalidRevision
        );
    }

    #[test]
    fn empty_or_duplicate_bind_plans_are_rejected() {
        let mut empty_config = config();
        Arc::get_mut(&mut empty_config).unwrap().bind_plan = BindPlan::default();
        assert_eq!(
            PreparedRuntime::prepare(empty_config, RuntimeRevision(1)).unwrap_err(),
            PrepareError::EmptyBindPlan
        );

        let mut config = config();
        let entries = config.bind_plan.entries.clone();
        Arc::get_mut(&mut config)
            .unwrap()
            .bind_plan
            .entries
            .extend(entries);
        assert_eq!(
            PreparedRuntime::prepare(config, RuntimeRevision(1)).unwrap_err(),
            PrepareError::DuplicateEndpoint { index: 1 }
        );
    }
}
