//! Resource refresh scheduler 与 coordinator 的 Runtime-facing 纯逻辑边界。
//!
//! 该模块编排 due、single-flight、backoff、CAS publish 和 stop 状态，并提供一次性的
//! remote fetch/parse/persist worker；timer 与 Runtime task 生命周期仍由调用方负责。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::config::resolve::{ConfigId, ResolvedHostsResource, ResolvedRuleSet};
use crate::ports::effects::ResourceFetcher;

use super::{
    FileFingerprint, HostsIndex, HostsLimits, LoadedHostsResource, LoadedRuleSetResource,
    RefreshBeginError, RefreshFailure, RefreshPermit, RefreshPublishError, RemoteResourceError,
    RemoteResourceOptions, ResourceRefreshCoordinator, ResourceRefreshPhase, ResourceRefreshStatus,
    ResourceRegistrySnapshot, ResourceSchedule, ResourceScheduleDecision, ResourceSchedulePolicy,
    ResourceSnapshot, ResourceSource, ResourceSourceKind, ResourceStaleStatus, RuleIndex,
    RuleLimits, fetch_remote_rule_set, load_hosts, load_rule_set,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceRefreshRuntimeBeginError {
    UnknownResource,
    NotDue(ResourceScheduleDecision),
    Refresh(RefreshBeginError),
}

/// 单资源 schedule 与 immutable registry coordinator 的组合 facade。
#[derive(Clone)]
pub struct ResourceRefreshRuntime<T> {
    coordinator: ResourceRefreshCoordinator<T>,
    schedules: Arc<Mutex<BTreeMap<ConfigId, ResourceSchedule>>>,
    policy: ResourceSchedulePolicy,
}

/// 绑定 schedule 的一次刷新 reservation。
#[derive(Clone)]
pub struct ResourceRefreshRuntimePermit {
    resource_id: ConfigId,
    permit: RefreshPermit,
}

impl ResourceRefreshRuntimePermit {
    pub fn resource_id(&self) -> &ConfigId {
        &self.resource_id
    }

    pub const fn epoch(&self) -> u64 {
        self.permit.epoch()
    }
}

impl<T> ResourceRefreshRuntime<T> {
    pub fn new(
        registry: ResourceRegistrySnapshot<T>,
        policy: ResourceSchedulePolicy,
        initial_due: u64,
    ) -> Self {
        let schedules = registry
            .summary()
            .into_iter()
            .map(|(resource_id, _)| (resource_id, ResourceSchedule::new(policy, initial_due)))
            .collect();
        Self {
            coordinator: ResourceRefreshCoordinator::new(registry, policy.backoff()),
            schedules: Arc::new(Mutex::new(schedules)),
            policy,
        }
    }

    pub fn current(&self) -> ResourceRegistrySnapshot<T>
    where
        T: Clone,
    {
        self.coordinator.current()
    }

    /// 从旧 Runtime 迁移稳定的资源版本和调度状态。
    ///
    /// 正在执行的 reservation 不会复制到候选，避免旧 task 在 reload 后继续向
    /// 新 Runtime 提交结果；已完成发布的 registry 版本和 schedule backoff 则按
    /// 资源过滤条件保留。
    pub fn merge_state_from<F>(&self, incoming: &Self, mut allowed: F)
    where
        T: Clone,
        F: FnMut(&ConfigId) -> bool,
    {
        self.coordinator
            .merge_newer_from(&incoming.coordinator, &mut allowed);

        let incoming_schedules = incoming
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (resource_id, incoming_schedule) in incoming_schedules {
            if allowed(&resource_id)
                && let Some(schedule) = schedules.get_mut(&resource_id)
            {
                schedule.merge_stable_state_from(incoming_schedule);
            }
        }
    }

    pub fn register(&self, resource_id: ConfigId, initial_due: u64) -> bool {
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if schedules.contains_key(&resource_id) {
            return false;
        }
        schedules.insert(resource_id, ResourceSchedule::new(self.policy, initial_due));
        true
    }

    pub fn decision(&self, resource_id: &ConfigId, now: u64) -> Option<ResourceScheduleDecision> {
        self.schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(resource_id)
            .map(|schedule| schedule.decision(now))
    }

    pub fn status(&self, resource_id: &ConfigId) -> ResourceRefreshStatus {
        self.coordinator.status(resource_id)
    }

    pub fn begin_due(
        &self,
        resource_id: &ConfigId,
        now: u64,
    ) -> Result<ResourceRefreshRuntimePermit, ResourceRefreshRuntimeBeginError> {
        let decision = self
            .decision(resource_id, now)
            .ok_or(ResourceRefreshRuntimeBeginError::UnknownResource)?;
        if !decision.is_due() {
            return Err(ResourceRefreshRuntimeBeginError::NotDue(decision));
        }
        let permit = self
            .coordinator
            .begin(resource_id, now)
            .map_err(ResourceRefreshRuntimeBeginError::Refresh)?;
        Ok(ResourceRefreshRuntimePermit {
            resource_id: resource_id.clone(),
            permit,
        })
    }

    /// 释放一次刷新 reservation，但保留 schedule，供取消或 publish 失败路径复用。
    pub fn abandon(&self, permit: ResourceRefreshRuntimePermit) -> bool {
        self.coordinator.cancel(&permit.resource_id, &permit.permit)
    }

    pub fn publish(
        &self,
        permit: ResourceRefreshRuntimePermit,
        candidate: ResourceSnapshot<T>,
        completed_at: u64,
    ) -> Result<(), RefreshPublishError> {
        self.coordinator.publish(&permit.permit, candidate)?;
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(schedule) = schedules.get_mut(&permit.resource_id) {
            let _ = schedule.record_success(completed_at);
        }
        Ok(())
    }

    pub fn fail(
        &self,
        permit: ResourceRefreshRuntimePermit,
        failed_at: u64,
    ) -> Result<RefreshFailure, RefreshPublishError> {
        let failure = self.coordinator.fail(&permit.permit, failed_at)?;
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(schedule) = schedules.get_mut(&permit.resource_id) {
            let _ = schedule.record_failure(failed_at);
        }
        Ok(failure)
    }

    pub fn cancel(&self, resource_id: &ConfigId, permit: ResourceRefreshRuntimePermit) -> bool {
        let cancelled = self.coordinator.cancel(resource_id, &permit.permit);
        if cancelled
            && let Some(schedule) = self
                .schedules
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_mut(resource_id)
        {
            let _ = schedule.cancel();
        }
        cancelled
    }

    pub fn shutdown(&self) {
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for schedule in schedules.values_mut() {
            let _ = schedule.shutdown();
        }
        drop(schedules);
        for resource_id in self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            let _ = self.coordinator.cancel_resource(&resource_id);
        }
    }

    pub fn phase(&self, resource_id: &ConfigId) -> ResourceRefreshPhase {
        self.status(resource_id).phase()
    }
}

#[derive(Debug, Error)]
pub enum ResourceRefreshWorkerError {
    #[error("resource `{resource}` is not a remote rule-set")]
    UnsupportedSource { resource: String },
    #[error("resource refresh could not begin: {0:?}")]
    Begin(ResourceRefreshRuntimeBeginError),
    #[error("remote resource fetch failed: {0}")]
    Fetch(#[source] RemoteResourceError),
    #[error("resource refresh candidate could not be published: {0:?}")]
    Publish(RefreshPublishError),
    #[error("resource refresh reservation could not be released: {0:?}")]
    Release(RefreshPublishError),
}

#[derive(Debug, Error)]
pub enum LocalResourceRefreshWorkerError {
    #[error("resource `{resource}` is not a file resource")]
    UnsupportedSource { resource: String },
    #[error("resource refresh could not begin: {0:?}")]
    Begin(ResourceRefreshRuntimeBeginError),
    #[error("hosts resource reload failed: {0}")]
    HostsLoad(#[source] super::ResourceLoadError),
    #[error("rule-set reload failed: {0}")]
    RuleSetLoad(#[source] super::RuleResourceLoadError),
    #[error("resource refresh candidate could not be published: {0:?}")]
    Publish(RefreshPublishError),
    #[error("resource refresh reservation could not be released: {0:?}")]
    Release(RefreshPublishError),
}

/// 将真实 remote rule-set fetch/parse/persist 接入 refresh reservation 的一次性 worker。
///
/// 该 worker 不创建 timer 或 supervisor task；调用方负责按 schedule 触发它，并把实际
/// worker 生命周期纳入 Runtime supervisor。
#[derive(Clone)]
pub struct ResourceRefreshWorker {
    runtime: ResourceRefreshRuntime<RuleIndex>,
    fetcher: Arc<dyn ResourceFetcher>,
}

impl ResourceRefreshWorker {
    pub fn new(
        runtime: ResourceRefreshRuntime<RuleIndex>,
        fetcher: Arc<dyn ResourceFetcher>,
    ) -> Self {
        Self { runtime, fetcher }
    }

    pub fn runtime(&self) -> &ResourceRefreshRuntime<RuleIndex> {
        &self.runtime
    }

    pub(crate) fn merge_state_from(&self, incoming: &Self) {
        self.runtime.merge_state_from(&incoming.runtime, |_| true);
    }

    pub(crate) fn current_snapshot(&self) -> Option<ResourceSnapshot<RuleIndex>> {
        let registry = self.runtime.current();
        let (resource_id, _) = registry.summary().into_iter().next()?;
        registry
            .lookup(&resource_id)
            .map(|snapshot| (*snapshot).clone())
    }

    pub async fn refresh_remote_rule_set(
        &self,
        resource: &ResolvedRuleSet,
        options: RemoteResourceOptions,
        now: u64,
    ) -> Result<super::LoadedRemoteRuleSet, ResourceRefreshWorkerError> {
        let resource_id = match resource {
            ResolvedRuleSet::Remote { id, .. } => id,
            ResolvedRuleSet::Const { id, .. } | ResolvedRuleSet::File { id, .. } => {
                return Err(ResourceRefreshWorkerError::UnsupportedSource {
                    resource: id.as_str().to_owned(),
                });
            }
        };
        let permit = self
            .runtime
            .begin_due(resource_id, now)
            .map_err(ResourceRefreshWorkerError::Begin)?;
        let cleanup = permit.clone();
        let loaded = match fetch_remote_rule_set(self.fetcher.as_ref(), resource, options).await {
            Ok(loaded) => loaded,
            Err(
                error @ (RemoteResourceError::Cancelled { .. }
                | RemoteResourceError::DeadlineExceeded),
            ) => {
                let _ = self.runtime.abandon(cleanup);
                return Err(ResourceRefreshWorkerError::Fetch(error));
            }
            Err(error) => {
                let _ = self.runtime.fail(cleanup, now);
                return Err(ResourceRefreshWorkerError::Fetch(error));
            }
        };
        let cleanup = permit.clone();
        let snapshot = loaded.snapshot();
        let candidate = ResourceSnapshot::new(
            snapshot.resource_id().clone(),
            permit.epoch(),
            snapshot.revision(),
            snapshot.content_hash().to_owned(),
            snapshot.source_fingerprint().to_owned(),
            snapshot.parser_version().to_owned(),
            snapshot.fetched_at(),
            snapshot.source_kind(),
            snapshot.used_fallback(),
            snapshot.stale_status(),
            snapshot.compiled().clone(),
        );
        let published = loaded.with_snapshot(candidate.clone());
        if let Err(error) = self.runtime.publish(permit, candidate, now) {
            if !self.runtime.abandon(cleanup) {
                return Err(ResourceRefreshWorkerError::Release(error));
            }
            return Err(ResourceRefreshWorkerError::Publish(error));
        }
        Ok(published)
    }
}

/// 受监督的 file hosts refresh worker。文件读取和解析仍由 Resource loader 所有，
/// worker 只负责把 bounded reload 绑定到 reservation 与 schedule 生命周期。
#[derive(Clone)]
pub struct FileHostsRefreshWorker {
    runtime: ResourceRefreshRuntime<HostsIndex>,
    resource: ResolvedHostsResource,
}

impl FileHostsRefreshWorker {
    pub fn new(
        runtime: ResourceRefreshRuntime<HostsIndex>,
        resource: ResolvedHostsResource,
    ) -> Self {
        Self { runtime, resource }
    }

    pub fn runtime(&self) -> &ResourceRefreshRuntime<HostsIndex> {
        &self.runtime
    }

    pub(crate) fn merge_state_from(&self, incoming: &Self) {
        self.runtime.merge_state_from(&incoming.runtime, |_| true);
    }

    pub(crate) fn current_snapshot(&self) -> Option<ResourceSnapshot<HostsIndex>> {
        let registry = self.runtime.current();
        let (resource_id, _) = registry.summary().into_iter().next()?;
        registry
            .lookup(&resource_id)
            .map(|snapshot| (*snapshot).clone())
    }

    pub fn refresh(
        &self,
        now: u64,
    ) -> Result<ResourceSnapshot<HostsIndex>, LocalResourceRefreshWorkerError> {
        let resource_id = match &self.resource {
            ResolvedHostsResource::File { id, .. } => id,
            ResolvedHostsResource::Const { id, .. } => {
                return Err(LocalResourceRefreshWorkerError::UnsupportedSource {
                    resource: id.as_str().to_owned(),
                });
            }
        };
        let permit = self
            .runtime
            .begin_due(resource_id, now)
            .map_err(LocalResourceRefreshWorkerError::Begin)?;
        let cleanup = permit.clone();
        let loaded = match load_hosts(&self.resource, HostsLimits::default()) {
            Ok(loaded) => loaded,
            Err(error) => {
                if let Err(release) = self.runtime.fail(cleanup, now) {
                    return Err(LocalResourceRefreshWorkerError::Release(release));
                }
                return Err(LocalResourceRefreshWorkerError::HostsLoad(error));
            }
        };
        let candidate = hosts_snapshot(&loaded, permit.epoch());
        if let Err(error) = self.runtime.publish(permit, candidate.clone(), now) {
            if !self.runtime.abandon(cleanup) {
                return Err(LocalResourceRefreshWorkerError::Release(error));
            }
            return Err(LocalResourceRefreshWorkerError::Publish(error));
        }
        Ok(candidate)
    }
}

/// 受监督的 file rule-set refresh worker。
#[derive(Clone)]
pub struct FileRuleSetRefreshWorker {
    runtime: ResourceRefreshRuntime<RuleIndex>,
    resource: ResolvedRuleSet,
}

impl FileRuleSetRefreshWorker {
    pub fn new(runtime: ResourceRefreshRuntime<RuleIndex>, resource: ResolvedRuleSet) -> Self {
        Self { runtime, resource }
    }

    pub fn runtime(&self) -> &ResourceRefreshRuntime<RuleIndex> {
        &self.runtime
    }

    pub(crate) fn merge_state_from(&self, incoming: &Self) {
        self.runtime.merge_state_from(&incoming.runtime, |_| true);
    }

    pub(crate) fn current_snapshot(&self) -> Option<ResourceSnapshot<RuleIndex>> {
        let registry = self.runtime.current();
        let (resource_id, _) = registry.summary().into_iter().next()?;
        registry
            .lookup(&resource_id)
            .map(|snapshot| (*snapshot).clone())
    }

    pub fn refresh(
        &self,
        now: u64,
    ) -> Result<ResourceSnapshot<RuleIndex>, LocalResourceRefreshWorkerError> {
        let resource_id = match &self.resource {
            ResolvedRuleSet::File { id, .. } => id,
            ResolvedRuleSet::Const { id, .. } | ResolvedRuleSet::Remote { id, .. } => {
                return Err(LocalResourceRefreshWorkerError::UnsupportedSource {
                    resource: id.as_str().to_owned(),
                });
            }
        };
        let permit = self
            .runtime
            .begin_due(resource_id, now)
            .map_err(LocalResourceRefreshWorkerError::Begin)?;
        let cleanup = permit.clone();
        let loaded = match load_rule_set(&self.resource, RuleLimits::default()) {
            Ok(loaded) => loaded,
            Err(error) => {
                if let Err(release) = self.runtime.fail(cleanup, now) {
                    return Err(LocalResourceRefreshWorkerError::Release(release));
                }
                return Err(LocalResourceRefreshWorkerError::RuleSetLoad(error));
            }
        };
        let candidate = rule_set_snapshot(&loaded, permit.epoch());
        if let Err(error) = self.runtime.publish(permit, candidate.clone(), now) {
            if !self.runtime.abandon(cleanup) {
                return Err(LocalResourceRefreshWorkerError::Release(error));
            }
            return Err(LocalResourceRefreshWorkerError::Publish(error));
        }
        Ok(candidate)
    }
}

fn hosts_snapshot(loaded: &LoadedHostsResource, epoch: u64) -> ResourceSnapshot<HostsIndex> {
    ResourceSnapshot::new(
        loaded.id().clone(),
        epoch,
        0,
        loaded.content_hash().to_owned(),
        source_fingerprint(loaded.source()),
        loaded.parser_version(),
        loaded.fetched_at(),
        ResourceSourceKind::File,
        false,
        ResourceStaleStatus::Fresh,
        loaded.index().clone(),
    )
}

fn rule_set_snapshot(loaded: &LoadedRuleSetResource, epoch: u64) -> ResourceSnapshot<RuleIndex> {
    ResourceSnapshot::new(
        loaded.id().clone(),
        epoch,
        0,
        loaded.content_hash().to_owned(),
        source_fingerprint(loaded.source()),
        loaded.parser_version(),
        loaded.fetched_at(),
        ResourceSourceKind::File,
        false,
        ResourceStaleStatus::Fresh,
        loaded.index().clone(),
    )
}

fn source_fingerprint(source: &ResourceSource) -> String {
    match source {
        ResourceSource::Const => "const".to_owned(),
        ResourceSource::File { fingerprint, .. } => file_fingerprint(fingerprint),
    }
}

fn file_fingerprint(fingerprint: &FileFingerprint) -> String {
    format!(
        "file:{}:{}",
        fingerprint.byte_len(),
        fingerprint.modified_unix_nanos().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::config::model::RuleSetFormat;
    use crate::config::resolve::ResolvedRuleSet;
    use crate::dns::{Cancellation, Deadline};
    use crate::ports::effects::{ResourceFetchRequest, ResourceFetchResult};
    use crate::ports::{PortError, PortFuture};
    use crate::resource::{
        CanonicalDomain, RefreshBackoff, ResourceScheduleStopReason, ResourceSourceKind,
        ResourceStaleStatus,
    };

    fn id(value: &str) -> ConfigId {
        ConfigId::new(value).expect("valid resource id")
    }

    fn policy() -> ResourceSchedulePolicy {
        ResourceSchedulePolicy::new(10, 3, 3, RefreshBackoff::new(2, 8).unwrap())
            .expect("valid schedule policy")
    }

    fn snapshot(name: &str, epoch: u64, value: &str) -> ResourceSnapshot<String> {
        ResourceSnapshot::new(
            id(name),
            epoch,
            1,
            format!("hash-{value}"),
            format!("fingerprint-{value}"),
            "parser-v1",
            UNIX_EPOCH,
            ResourceSourceKind::File,
            false,
            ResourceStaleStatus::Fresh,
            value.to_owned(),
        )
    }

    fn runtime() -> ResourceRefreshRuntime<String> {
        let registry = ResourceRegistrySnapshot::new()
            .publish(snapshot("hosts", 1, "old"))
            .unwrap();
        ResourceRefreshRuntime::new(registry, policy(), 0)
    }

    fn rule_runtime() -> ResourceRefreshRuntime<RuleIndex> {
        let index = RuleIndex::parse("DOMAIN-SUFFIX,old.test\n", RuleSetFormat::Clash).unwrap();
        let snapshot = ResourceSnapshot::new(
            id("remote-rules"),
            1,
            1,
            "old-hash",
            "old-fingerprint",
            "rule-index-v1",
            UNIX_EPOCH,
            ResourceSourceKind::Remote,
            false,
            ResourceStaleStatus::Fresh,
            index,
        );
        ResourceRefreshRuntime::new(
            ResourceRegistrySnapshot::new().publish(snapshot).unwrap(),
            policy(),
            0,
        )
    }

    struct FakeResourceFetcher {
        body: Arc<[u8]>,
        calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for FakeResourceFetcher {
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

    fn remote_rule_set() -> ResolvedRuleSet {
        ResolvedRuleSet::Remote {
            id: id("remote-rules"),
            format: RuleSetFormat::Clash,
            url: url::Url::parse("https://rules.example.test/list").unwrap(),
            proxy: None,
            auto_update: true,
            update_interval: Some(Duration::from_secs(60)),
        }
    }

    fn remote_options() -> RemoteResourceOptions {
        let root = std::env::temp_dir().join(format!("fluxdns-worker-{}", std::process::id()));
        RemoteResourceOptions::new(
            1024,
            root.join("rules.txt"),
            root.join("rules.manifest"),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
    }

    #[tokio::test]
    async fn worker_fetches_parses_persists_and_publishes_remote_rule_set() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker = ResourceRefreshWorker::new(
            rule_runtime(),
            Arc::new(FakeResourceFetcher {
                body: Arc::from(&b"DOMAIN-SUFFIX,new.test\n"[..]),
                calls: Arc::clone(&calls),
            }),
        );
        let loaded = worker
            .refresh_remote_rule_set(&remote_rule_set(), remote_options(), 0)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(loaded.snapshot().epoch(), 2);
        assert_eq!(
            worker
                .runtime()
                .current()
                .lookup(&id("remote-rules"))
                .unwrap()
                .compiled()
                .suffix_count(),
            1
        );
        assert_eq!(
            worker.runtime().phase(&id("remote-rules")),
            ResourceRefreshPhase::Idle
        );
    }

    #[tokio::test]
    async fn worker_records_parse_failure_and_releases_reservation() {
        let worker = ResourceRefreshWorker::new(
            rule_runtime(),
            Arc::new(FakeResourceFetcher {
                body: Arc::from(&b"not-a-valid-rule\n"[..]),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        let error = worker
            .refresh_remote_rule_set(&remote_rule_set(), remote_options(), 0)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ResourceRefreshWorkerError::Fetch(RemoteResourceError::Parse(_))
        ));
        assert_eq!(
            worker.runtime().phase(&id("remote-rules")),
            ResourceRefreshPhase::Backoff {
                retry_not_before: 2,
                consecutive_failures: 1,
            }
        );
        assert!(worker.runtime().begin_due(&id("remote-rules"), 1).is_err());
    }

    #[test]
    fn file_hosts_worker_reloads_and_publishes_new_snapshot() {
        let path = std::env::temp_dir().join(format!(
            "fluxdns-file-worker-{}-{}.hosts",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "192.0.2.10 old.example\n").unwrap();
        let resource = ResolvedHostsResource::File {
            id: id("file-hosts"),
            format: crate::config::model::HostsFormat::Hosts,
            path: path.clone(),
            auto_update: true,
            update_interval: Some(Duration::from_secs(10)),
        };
        let initial = load_hosts(&resource, HostsLimits::default()).unwrap();
        let initial_snapshot = ResourceSnapshot::new(
            initial.id().clone(),
            1,
            1,
            initial.content_hash().to_owned(),
            source_fingerprint(initial.source()),
            initial.parser_version(),
            initial.fetched_at(),
            ResourceSourceKind::File,
            false,
            ResourceStaleStatus::Fresh,
            initial.index().clone(),
        );
        let runtime = ResourceRefreshRuntime::new(
            ResourceRegistrySnapshot::new()
                .publish(initial_snapshot)
                .unwrap(),
            policy(),
            0,
        );
        let worker = FileHostsRefreshWorker::new(runtime, resource);

        std::fs::write(&path, "192.0.2.11 new.example\n").unwrap();
        let refreshed = worker.refresh(0).unwrap();
        assert_eq!(refreshed.epoch(), 2);
        assert_eq!(
            refreshed
                .compiled()
                .records(&CanonicalDomain::parse("new.example").unwrap())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            worker
                .runtime()
                .current()
                .lookup(&id("file-hosts"))
                .unwrap()
                .source_kind(),
            ResourceSourceKind::File
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_rule_set_worker_reloads_and_publishes_new_snapshot() {
        let path = std::env::temp_dir().join(format!(
            "fluxdns-file-rule-worker-{}-{}.rules",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "DOMAIN-SUFFIX,old.example\n").unwrap();
        let resource = ResolvedRuleSet::File {
            id: id("file-rules"),
            format: RuleSetFormat::Clash,
            path: path.clone(),
            auto_update: true,
            update_interval: Some(Duration::from_secs(10)),
        };
        let initial = load_rule_set(&resource, RuleLimits::default()).unwrap();
        let initial_snapshot = ResourceSnapshot::new(
            initial.id().clone(),
            1,
            1,
            initial.content_hash().to_owned(),
            source_fingerprint(initial.source()),
            initial.parser_version(),
            initial.fetched_at(),
            ResourceSourceKind::File,
            false,
            ResourceStaleStatus::Fresh,
            initial.index().clone(),
        );
        let runtime = ResourceRefreshRuntime::new(
            ResourceRegistrySnapshot::new()
                .publish(initial_snapshot)
                .unwrap(),
            policy(),
            0,
        );
        let worker = FileRuleSetRefreshWorker::new(runtime, resource);

        std::fs::write(&path, "DOMAIN-SUFFIX,new.example\n").unwrap();
        let refreshed = worker.refresh(0).unwrap();
        assert_eq!(refreshed.epoch(), 2);
        assert!(
            refreshed
                .compiled()
                .matches(&CanonicalDomain::parse("new.example").unwrap())
                .is_some()
        );
        assert!(
            worker
                .runtime()
                .current()
                .lookup(&id("file-rules"))
                .is_some()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn due_reservation_publishes_and_reschedules() {
        let runtime = runtime();
        let permit = runtime.begin_due(&id("hosts"), 0).unwrap();
        assert_eq!(permit.resource_id(), &id("hosts"));
        assert_eq!(permit.epoch(), 2);
        assert_eq!(
            runtime.status(&id("hosts")).phase(),
            ResourceRefreshPhase::Refreshing { epoch: 2 }
        );

        runtime
            .publish(permit, snapshot("hosts", 2, "new"), 5)
            .unwrap();
        assert_eq!(
            runtime.current().lookup(&id("hosts")).unwrap().compiled(),
            "new"
        );
        assert_eq!(
            runtime.decision(&id("hosts"), 14).unwrap().next_due(),
            Some(15)
        );
        assert_eq!(
            runtime.status(&id("hosts")).phase(),
            ResourceRefreshPhase::Idle
        );
    }

    #[test]
    fn failure_backoff_blocks_until_retry_deadline() {
        let runtime = runtime();
        let permit = runtime.begin_due(&id("hosts"), 0).unwrap();
        let failure = runtime.fail(permit, 0).unwrap();
        assert_eq!(failure.retry_not_before(), 2);
        assert!(matches!(
            runtime.begin_due(&id("hosts"), 1),
            Err(ResourceRefreshRuntimeBeginError::NotDue(
                ResourceScheduleDecision::Waiting {
                    next_due: 2,
                    consecutive_failures: 1,
                    stale: false,
                }
            ))
        ));
        assert!(runtime.begin_due(&id("hosts"), 2).is_ok());
    }

    #[test]
    fn cancel_releases_reservation_and_stops_future_work() {
        let runtime = runtime();
        let permit = runtime.begin_due(&id("hosts"), 0).unwrap();
        assert!(runtime.cancel(&id("hosts"), permit));
        assert_eq!(
            runtime.decision(&id("hosts"), 100),
            Some(ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Cancelled,
            })
        );
        assert!(matches!(
            runtime.begin_due(&id("hosts"), 100),
            Err(ResourceRefreshRuntimeBeginError::NotDue(
                ResourceScheduleDecision::Stopped {
                    reason: ResourceScheduleStopReason::Cancelled,
                }
            ))
        ));
    }

    #[test]
    fn shutdown_stops_all_registered_resources_and_clears_inflight() {
        let runtime = runtime();
        assert!(runtime.register(id("rules"), 0));
        let permit = runtime.begin_due(&id("hosts"), 0).unwrap();
        runtime.shutdown();
        assert!(matches!(
            runtime.decision(&id("hosts"), 0),
            Some(ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Shutdown,
            })
        ));
        assert!(matches!(
            runtime.decision(&id("rules"), 0),
            Some(ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Shutdown,
            })
        ));
        assert!(!runtime.cancel(&id("hosts"), permit));
    }
}
