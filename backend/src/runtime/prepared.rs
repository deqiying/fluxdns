use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use thiserror::Error;

use crate::config::{BindPlan, ResolvedConfig};
use crate::dns::{Cancellation, DEFAULT_LOCAL_TTL, Deadline, PolicyDnsCore, RuntimeRevision};
use crate::ports::effects::ResourceFetcher;
use crate::resource::{
    RemoteResourceOptions, ReqwestResourceFetcher, ResourceSnapshot, RuleIndex,
    fetch_remote_rule_set, restore_remote_rule_set,
};

use super::RuntimeSnapshot;

/// 没有对外 socket 的候选运行时。
pub struct PreparedRuntime {
    pub(crate) snapshot: Arc<RuntimeSnapshot>,
    pub(crate) bind_plan: Arc<BindPlan>,
    resource_fetcher: Option<Arc<dyn ResourceFetcher>>,
    resource_snapshots: BTreeMap<crate::config::resolve::ConfigId, ResourceSnapshot<RuleIndex>>,
}

impl PreparedRuntime {
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
        let resource_fetcher = Arc::new(
            ReqwestResourceFetcher::from_resolved(&config.outbounds, 64 * 1024).map_err(
                |error| PrepareError::ResourceFetcher {
                    reason: error.to_string(),
                },
            )?,
        );
        let mut remote_snapshots = BTreeMap::new();
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
            remote_snapshots.insert(id.clone(), loaded.snapshot().clone());
        }
        let policy_core = PolicyDnsCore::from_config_with_rule_snapshots(
            &config,
            DEFAULT_LOCAL_TTL,
            &remote_snapshots,
        )
        .map_err(|error| PrepareError::PolicyCore {
            reason: error.to_string(),
        })?;
        let snapshot = Arc::new(RuntimeSnapshot::with_policy_core(
            revision,
            config,
            policy_core,
        ));
        Ok(Self {
            snapshot,
            bind_plan: Arc::new(bind_plan),
            resource_fetcher: Some(resource_fetcher),
            resource_snapshots: remote_snapshots,
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

    pub fn resource_snapshots(
        &self,
    ) -> &BTreeMap<crate::config::resolve::ConfigId, ResourceSnapshot<RuleIndex>> {
        &self.resource_snapshots
    }

    pub fn preflight(&self) -> PreflightReport {
        PreflightReport {
            revision: self.snapshot.revision(),
            endpoint_count: self.bind_plan.entries.len(),
            normalized_hash: self.snapshot.config().normalized_hash.clone(),
            has_policy_core: self.snapshot.policy_core().is_some(),
            has_resource_fetcher: self.resource_fetcher.is_some(),
            resource_snapshot_count: self.resource_snapshots.len(),
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
            .field("resource_snapshot_count", &self.resource_snapshots.len())
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

    use crate::config::resolve::{ConfigId, ResolvedUpstream};
    use crate::config::{BindPlan, ConfigLoader, LoadOptions};
    use crate::dns::{Cancellation, Deadline, RuntimeRevision};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{PrepareError, PreparedRuntime};

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
    auto_update: false
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
    async fn async_prepare_restores_or_fetches_remote_rule_set_before_bind() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
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
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 27\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nDOMAIN-SUFFIX,example.test\n",
                )
                .await
                .unwrap();
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
