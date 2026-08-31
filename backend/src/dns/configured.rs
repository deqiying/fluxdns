//! 从 ResolvedConfig 组装当前可用的 DNS Core。

use thiserror::Error;

use crate::config::model::HostsFormat;
use crate::config::resolve::{ResolvedConfig, ResolvedHostsResource, ResolvedUpstream};
use crate::ports::PortFuture;

use super::{CoreError, CoreOutcome, DnsCore, DnsRequest, HostsCore, HostsTable, ServFailCore};

pub const DEFAULT_LOCAL_TTL: u32 = 60;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum CoreBuildError {
    #[error("hosts resource `{resource}` could not be parsed")]
    InvalidHosts { resource: String },
}

/// 当前配置可装配出的最小 Core；未实现资源格式时安全降级为 SERVFAIL。
#[derive(Clone, Debug)]
pub enum ConfiguredDnsCore {
    Hosts(HostsCore),
    ServFail(ServFailCore),
}

impl ConfiguredDnsCore {
    pub fn from_config(config: &ResolvedConfig) -> Result<Self, CoreBuildError> {
        let mut table = HostsTable::parse("").expect("empty hosts table is valid");
        for resource in &config.hosts {
            let ResolvedHostsResource::Const { id, format, hosts } = resource else {
                continue;
            };
            if *format != HostsFormat::Hosts {
                continue;
            }
            let parsed = HostsTable::parse(hosts).map_err(|_| CoreBuildError::InvalidHosts {
                resource: id.as_str().to_owned(),
            })?;
            table.merge(parsed);
        }
        for upstream in &config.upstreams {
            let ResolvedUpstream::Hosts { id, format, hosts } = upstream else {
                continue;
            };
            if !format.eq_ignore_ascii_case("hosts") {
                continue;
            }
            let parsed = HostsTable::parse(hosts).map_err(|_| CoreBuildError::InvalidHosts {
                resource: id.as_str().to_owned(),
            })?;
            table.merge(parsed);
        }

        if table.is_empty() {
            Ok(Self::ServFail(ServFailCore))
        } else {
            Ok(Self::Hosts(HostsCore::new(table, DEFAULT_LOCAL_TTL)))
        }
    }

    pub fn has_local_hosts(&self) -> bool {
        matches!(self, Self::Hosts(_))
    }
}

impl DnsCore for ConfiguredDnsCore {
    fn resolve<'a>(
        &'a self,
        request: &'a DnsRequest,
    ) -> PortFuture<'a, Result<CoreOutcome, CoreError>> {
        match self {
            Self::Hosts(core) => core.resolve(request),
            Self::ServFail(core) => core.resolve(request),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::config::{ConfigLoader, LoadOptions};

    use super::{ConfiguredDnsCore, DEFAULT_LOCAL_TTL};

    fn example_config() -> Arc<crate::config::ResolvedConfig> {
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(include_str!("../../../config-example.yaml"))
            .expect("example config must remain valid")
            .resolved
    }

    #[test]
    fn selects_hosts_core_from_supported_inline_resource() {
        let config = example_config();
        let core = ConfiguredDnsCore::from_config(&config).unwrap();

        assert!(core.has_local_hosts());
        assert_eq!(DEFAULT_LOCAL_TTL, 60);
    }

    #[test]
    fn falls_back_to_servfail_when_no_supported_inline_resource_exists() {
        let mut config = example_config();
        let config = Arc::get_mut(&mut config).expect("config fixture must be uniquely owned");
        config.hosts.clear();
        config.upstreams.retain(|upstream| {
            !matches!(
                upstream,
                crate::config::resolve::ResolvedUpstream::Hosts { .. }
            )
        });

        let core = ConfiguredDnsCore::from_config(&config).unwrap();
        assert!(!core.has_local_hosts());
    }
}
