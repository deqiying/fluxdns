use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use thiserror::Error;

use crate::config::{BindPlan, ResolvedConfig};
use crate::dns::{DEFAULT_LOCAL_TTL, PolicyDnsCore, RuntimeRevision};

use super::RuntimeSnapshot;

/// 没有对外 socket 的候选运行时。
pub struct PreparedRuntime {
    pub(crate) snapshot: Arc<RuntimeSnapshot>,
    pub(crate) bind_plan: Arc<BindPlan>,
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
        })
    }

    /// 在 socket bind 前完成 Policy/Resource 本地 Core 构建，并把 handle 固定进 snapshot。
    pub fn prepare_with_policy_core(
        config: Arc<ResolvedConfig>,
        revision: RuntimeRevision,
    ) -> Result<Self, PrepareError> {
        let bind_plan = prepare_bind_plan(&config, revision)?;
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
        })
    }

    pub fn snapshot(&self) -> &Arc<RuntimeSnapshot> {
        &self.snapshot
    }

    pub fn bind_plan(&self) -> &BindPlan {
        &self.bind_plan
    }

    pub fn preflight(&self) -> PreflightReport {
        PreflightReport {
            revision: self.snapshot.revision(),
            endpoint_count: self.bind_plan.entries.len(),
            normalized_hash: self.snapshot.config().normalized_hash.clone(),
            has_policy_core: self.snapshot.policy_core().is_some(),
        }
    }
}

impl fmt::Debug for PreparedRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedRuntime")
            .field("snapshot", &self.snapshot)
            .field("bind_entry_count", &self.bind_plan.entries.len())
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
}

/// prepare 成功后可用于观测和验收的最小摘要。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightReport {
    pub revision: RuntimeRevision,
    pub endpoint_count: usize,
    pub normalized_hash: String,
    pub has_policy_core: bool,
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

    use crate::config::resolve::{ConfigId, ResolvedUpstream};
    use crate::config::{BindPlan, ConfigLoader, LoadOptions};
    use crate::dns::RuntimeRevision;

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

    #[test]
    fn prepare_creates_a_candidate_without_binding() {
        let config = config();
        let candidate = PreparedRuntime::prepare(Arc::clone(&config), RuntimeRevision(1)).unwrap();

        assert_eq!(candidate.preflight().endpoint_count, 1);
        assert_eq!(candidate.snapshot().revision(), RuntimeRevision(1));
        assert_eq!(candidate.bind_plan().entries.len(), 1);
        assert!(Arc::ptr_eq(&candidate.snapshot().config_arc(), &config));
        assert!(!candidate.preflight().has_policy_core);
        assert!(candidate.snapshot().dns_core().is_none());
    }

    #[test]
    fn prepare_with_policy_core_captures_one_immutable_core() {
        let candidate =
            PreparedRuntime::prepare_with_policy_core(config(), RuntimeRevision(2)).unwrap();

        assert!(candidate.preflight().has_policy_core);
        assert!(candidate.snapshot().policy_core().is_some());
        assert!(candidate.snapshot().dns_core().is_some());
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
