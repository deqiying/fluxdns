use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use arc_swap::ArcSwap;

use crate::config::ResolvedConfig;
use crate::config::resolve::{ResolvedHostsResource, ResolvedRuleSet};
use crate::dns::{DnsCore, PolicyDnsCore, RuntimeRevision};
use crate::resource::{
    ResourcePublishError, ResourceRegistrySnapshot, ResourceSnapshot, ResourceSourceKind,
    ResourceStaleStatus,
};

/// 请求热路径使用的不可变运行时输入。
///
/// 阶段 3 的首个切片先承载已经归一化的配置；策略、资源、上游和缓存
/// registry 会在对应模块实现后以同样的不可变句柄加入，而不会重新解析 YAML。
pub struct RuntimeSnapshot {
    revision: RuntimeRevision,
    config: Arc<ResolvedConfig>,
    policy_core: Option<Arc<PolicyDnsCore>>,
    resources: Arc<ArcSwap<ResourceRegistrySnapshot<()>>>,
}

impl RuntimeSnapshot {
    pub(crate) fn new(revision: RuntimeRevision, config: Arc<ResolvedConfig>) -> Self {
        Self {
            revision,
            resources: Arc::new(ArcSwap::from_pointee(resource_snapshot(revision, &config))),
            config,
            policy_core: None,
        }
    }

    pub(crate) fn with_policy_core(
        revision: RuntimeRevision,
        config: Arc<ResolvedConfig>,
        policy_core: PolicyDnsCore,
    ) -> Self {
        Self {
            revision,
            resources: Arc::new(ArcSwap::from_pointee(resource_snapshot(revision, &config))),
            config,
            policy_core: Some(Arc::new(policy_core)),
        }
    }

    pub(crate) fn with_policy_core_and_resources(
        revision: RuntimeRevision,
        config: Arc<ResolvedConfig>,
        policy_core: PolicyDnsCore,
        host_snapshots: impl IntoIterator<Item = ResourceSnapshot<crate::resource::HostsIndex>>,
        rule_snapshots: impl IntoIterator<Item = ResourceSnapshot<crate::resource::RuleIndex>>,
    ) -> Self {
        let mut resources = resource_snapshot(revision, &config);
        for snapshot in host_snapshots {
            resources = resources.replace(resource_metadata(&snapshot));
        }
        for snapshot in rule_snapshots {
            resources = resources.replace(resource_metadata(&snapshot));
        }
        Self {
            revision,
            resources: Arc::new(ArcSwap::from_pointee(resources)),
            config,
            policy_core: Some(Arc::new(policy_core)),
        }
    }

    pub fn revision(&self) -> RuntimeRevision {
        self.revision
    }

    pub fn config(&self) -> &ResolvedConfig {
        &self.config
    }

    pub fn config_arc(&self) -> Arc<ResolvedConfig> {
        Arc::clone(&self.config)
    }

    pub fn policy_core(&self) -> Option<&PolicyDnsCore> {
        self.policy_core.as_deref()
    }

    pub(crate) fn policy_core_arc(&self) -> Option<Arc<PolicyDnsCore>> {
        self.policy_core.as_ref().map(Arc::clone)
    }

    /// 克隆与本 snapshot 同 revision 的不可变 DNS Core handle。
    pub fn dns_core(&self) -> Option<Arc<dyn DnsCore>> {
        self.policy_core
            .as_ref()
            .map(|core| Arc::clone(core) as Arc<dyn DnsCore>)
    }

    pub fn resources(&self) -> Arc<ResourceRegistrySnapshot<()>> {
        self.resources.load_full()
    }

    /// 将旧 Runtime 中仍与候选配置兼容的更高 metadata 版本合并到候选。
    pub(crate) fn merge_resource_metadata_from<F>(&self, incoming: &Self, mut allowed: F)
    where
        F: FnMut(&crate::config::resolve::ConfigId) -> bool,
    {
        let incoming = incoming.resources.load_full();
        loop {
            let current = self.resources.load_full();
            let next = current.merge_newer_from(&incoming, &mut allowed);
            if current.summary() == next.summary() {
                return;
            }
            let observed = self.resources.compare_and_swap(&current, Arc::new(next));
            if Arc::ptr_eq(&*observed, &current) {
                return;
            }
        }
    }

    pub(crate) fn publish_resource<T>(
        &self,
        snapshot: &ResourceSnapshot<T>,
    ) -> Result<(), ResourcePublishError> {
        let candidate = ResourceSnapshot::new(
            snapshot.resource_id().clone(),
            snapshot.epoch(),
            snapshot.revision(),
            snapshot.content_hash().to_owned(),
            snapshot.source_fingerprint().to_owned(),
            snapshot.parser_version().to_owned(),
            snapshot.fetched_at(),
            snapshot.source_kind(),
            snapshot.used_fallback(),
            snapshot.stale_status(),
            (),
        );
        loop {
            let current = self.resources.load_full();
            if let Some(existing) = current.lookup(candidate.resource_id())
                && existing.version() >= candidate.version()
            {
                return Ok(());
            }
            let expected = current
                .lookup(candidate.resource_id())
                .map(|existing| existing.version());
            let next = current.compare_and_publish(expected, candidate.clone())?;
            let observed = self.resources.compare_and_swap(&current, Arc::new(next));
            if Arc::ptr_eq(&*observed, &current) {
                return Ok(());
            }
        }
    }

    pub fn summary(&self) -> RuntimeSnapshotSummary {
        RuntimeSnapshotSummary {
            revision: self.revision,
            normalized_hash: self.config.normalized_hash.clone(),
            listener_count: self.config.listeners.len(),
            bind_entry_count: self.config.bind_plan.entries.len(),
            resource_count: self.resources.load().len(),
            has_policy_core: self.policy_core.is_some(),
        }
    }
}

impl fmt::Debug for RuntimeSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeSnapshot")
            .field("revision", &self.revision)
            .field("config", &self.config)
            .field("has_policy_core", &self.policy_core.is_some())
            .finish()
    }
}

/// 可安全用于日志和测试断言的 snapshot 摘要。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSnapshotSummary {
    pub revision: RuntimeRevision,
    pub normalized_hash: String,
    pub listener_count: usize,
    pub bind_entry_count: usize,
    pub resource_count: usize,
    pub has_policy_core: bool,
}

fn resource_snapshot(
    revision: RuntimeRevision,
    config: &ResolvedConfig,
) -> ResourceRegistrySnapshot<()> {
    let mut registry = ResourceRegistrySnapshot::new();
    for resource in &config.hosts {
        let (id, source_kind) = match resource {
            ResolvedHostsResource::Const { id, .. } => (id, ResourceSourceKind::Const),
            ResolvedHostsResource::File { id, .. } => (id, ResourceSourceKind::File),
        };
        registry = publish_resource_entry(
            registry,
            resource_entry(id, source_kind, revision, &config.normalized_hash),
        );
    }
    for resource in &config.rule_sets {
        let (id, source_kind) = match resource {
            ResolvedRuleSet::Const { id, .. } => (id, ResourceSourceKind::Const),
            ResolvedRuleSet::File { id, .. } => (id, ResourceSourceKind::File),
            ResolvedRuleSet::Remote { id, .. } => (id, ResourceSourceKind::Remote),
        };
        registry = publish_resource_entry(
            registry,
            resource_entry(id, source_kind, revision, &config.normalized_hash),
        );
    }
    registry
}

fn publish_resource_entry(
    registry: ResourceRegistrySnapshot<()>,
    candidate: ResourceSnapshot<()>,
) -> ResourceRegistrySnapshot<()> {
    match registry.publish(candidate) {
        Ok(next) => next,
        Err(ResourcePublishError::StaleEpoch { .. }) => registry,
        Err(ResourcePublishError::CompareAndSwapFailed { .. }) => registry,
    }
}

fn resource_entry(
    id: &crate::config::resolve::ConfigId,
    source_kind: ResourceSourceKind,
    _revision: RuntimeRevision,
    normalized_hash: &str,
) -> ResourceSnapshot<()> {
    ResourceSnapshot::new(
        id.clone(),
        0,
        0,
        normalized_hash,
        "config",
        "config-v1",
        SystemTime::now(),
        source_kind,
        false,
        ResourceStaleStatus::Fresh,
        (),
    )
}

fn resource_metadata<T>(snapshot: &ResourceSnapshot<T>) -> ResourceSnapshot<()> {
    ResourceSnapshot::new(
        snapshot.resource_id().clone(),
        snapshot.epoch(),
        snapshot.revision(),
        snapshot.content_hash().to_owned(),
        snapshot.source_fingerprint().to_owned(),
        snapshot.parser_version().to_owned(),
        snapshot.fetched_at(),
        snapshot.source_kind(),
        snapshot.used_fallback(),
        snapshot.stale_status(),
        (),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::RuntimeRevision;

    use super::RuntimeSnapshot;

    fn config() -> Arc<crate::config::ResolvedConfig> {
        let work_path = crate::config::test_support::absolute_path("runtime-snapshot");
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

    #[test]
    fn snapshot_keeps_one_revision_and_redacted_config_handle() {
        let config = config();
        let expected_hash = config.normalized_hash.clone();
        let snapshot = RuntimeSnapshot::new(RuntimeRevision(7), Arc::clone(&config));

        assert_eq!(snapshot.revision(), RuntimeRevision(7));
        assert!(Arc::ptr_eq(&snapshot.config_arc(), &config));
        assert_eq!(
            snapshot.summary(),
            super::RuntimeSnapshotSummary {
                revision: RuntimeRevision(7),
                normalized_hash: expected_hash,
                listener_count: 1,
                bind_entry_count: 1,
                resource_count: 1,
                has_policy_core: false,
            }
        );
        assert!(snapshot.dns_core().is_none());
        assert!(format!("{snapshot:?}").contains("RuntimeSnapshot"));
    }

    // V1-P02：两个资源反复同时进入 metadata CAS，迟到的同资源版本不能回退或丢失兄弟项。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn contract_v1_concurrent_metadata_cas_keeps_both_resources_monotonic() {
        use crate::config::resolve::ConfigId;
        use crate::resource::{
            ResourceSnapshot, ResourceSourceKind, ResourceStaleStatus, ResourceVersion,
        };
        use std::time::{Duration, UNIX_EPOCH};
        let snapshot = Arc::new(RuntimeSnapshot::new(RuntimeRevision(1), config()));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut tasks = tokio::task::JoinSet::new();
        for name in ["left", "right"] {
            let snapshot = Arc::clone(&snapshot);
            let barrier = Arc::clone(&barrier);
            tasks.spawn(async move {
                let id = ConfigId::new(name).unwrap();
                for epoch in 2..=33 {
                    let resource = ResourceSnapshot::new(
                        id.clone(),
                        epoch,
                        1,
                        format!("{name}-{epoch}"),
                        "source",
                        "parser",
                        UNIX_EPOCH,
                        ResourceSourceKind::File,
                        false,
                        ResourceStaleStatus::Fresh,
                        (),
                    );
                    barrier.wait().await;
                    snapshot.publish_resource(&resource).unwrap();
                    let stale = ResourceSnapshot::new(
                        id.clone(),
                        epoch - 1,
                        1,
                        "stale",
                        "source",
                        "parser",
                        UNIX_EPOCH,
                        ResourceSourceKind::File,
                        false,
                        ResourceStaleStatus::Fresh,
                        (),
                    );
                    snapshot.publish_resource(&stale).unwrap();
                    assert_eq!(
                        snapshot.resources().lookup(&id).unwrap().version(),
                        ResourceVersion::new(epoch, 1)
                    );
                }
            });
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(task) = tasks.join_next().await {
                task.unwrap();
            }
        })
        .await
        .expect("metadata CAS watchdog expired");
        for name in ["left", "right"] {
            let resources = snapshot.resources();
            let resource = resources.lookup(&ConfigId::new(name).unwrap()).unwrap();
            assert_eq!(resource.version(), ResourceVersion::new(33, 1));
            assert_eq!(resource.content_hash(), format!("{name}-33"));
        }
        assert_eq!(snapshot.revision(), RuntimeRevision(1));
    }
}
