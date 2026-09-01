use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

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
    resources: Arc<ResourceRegistrySnapshot<()>>,
}

impl RuntimeSnapshot {
    pub(crate) fn new(revision: RuntimeRevision, config: Arc<ResolvedConfig>) -> Self {
        Self {
            revision,
            resources: Arc::new(resource_snapshot(revision, &config)),
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
            resources: Arc::new(resource_snapshot(revision, &config)),
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

    /// 克隆与本 snapshot 同 revision 的不可变 DNS Core handle。
    pub fn dns_core(&self) -> Option<Arc<dyn DnsCore>> {
        self.policy_core
            .as_ref()
            .map(|core| Arc::clone(core) as Arc<dyn DnsCore>)
    }

    pub fn resources(&self) -> &ResourceRegistrySnapshot<()> {
        &self.resources
    }

    pub fn summary(&self) -> RuntimeSnapshotSummary {
        RuntimeSnapshotSummary {
            revision: self.revision,
            normalized_hash: self.config.normalized_hash.clone(),
            listener_count: self.config.listeners.len(),
            bind_entry_count: self.config.bind_plan.entries.len(),
            resource_count: self.resources.len(),
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
    revision: RuntimeRevision,
    normalized_hash: &str,
) -> ResourceSnapshot<()> {
    ResourceSnapshot::new(
        id.clone(),
        revision.0,
        1,
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::RuntimeRevision;

    use super::RuntimeSnapshot;

    fn config() -> Arc<crate::config::ResolvedConfig> {
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(
                r#"
version: 1
work:
  path: /tmp/fluxdns-runtime-test
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
dns: {}
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
            )
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
}
