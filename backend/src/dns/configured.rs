//! 从 ResolvedConfig 组装当前可用的 DNS Core。

use thiserror::Error;

use crate::config::resolve::{ResolvedConfig, ResolvedHostsResource};
use crate::ports::PortFuture;
use crate::resource::{HostsLimits, ResourceLoadError, load_hosts};

use super::{CoreError, CoreOutcome, DnsCore, DnsRequest, HostsCore, ServFailCore};

pub const DEFAULT_LOCAL_TTL: u32 = 60;

#[derive(Debug, Error)]
pub enum CoreBuildError {
    #[error("hosts resource `{resource}` could not be parsed")]
    InvalidHosts { resource: String },
    #[error("hosts resource `{resource}` could not be loaded: {source}")]
    ResourceLoad {
        resource: String,
        #[source]
        source: ResourceLoadError,
    },
}

/// 当前配置可装配出的最小 Core；未实现资源格式时安全降级为 SERVFAIL。
#[derive(Clone, Debug)]
pub enum ConfiguredDnsCore {
    Hosts(HostsCore),
    ServFail(ServFailCore),
}

impl ConfiguredDnsCore {
    pub fn from_config(config: &ResolvedConfig) -> Result<Self, CoreBuildError> {
        let mut indexes = Vec::new();
        for resource in &config.hosts {
            let resource_id = match resource {
                ResolvedHostsResource::Const { id, .. }
                | ResolvedHostsResource::File { id, .. } => id.as_str().to_owned(),
            };
            let loaded =
                load_hosts(resource, HostsLimits::default()).map_err(|source| match source {
                    ResourceLoadError::Parse { resource, .. } => {
                        CoreBuildError::InvalidHosts { resource }
                    }
                    source => CoreBuildError::ResourceLoad {
                        resource: resource_id,
                        source,
                    },
                })?;
            if !loaded.index().is_empty() {
                indexes.push(loaded.index().clone());
            }
        }
        if indexes.is_empty() {
            Ok(Self::ServFail(ServFailCore))
        } else {
            Ok(Self::Hosts(HostsCore::from_resource_indexes(
                indexes,
                DEFAULT_LOCAL_TTL,
            )))
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
    use std::fs;
    use std::sync::Arc;

    use crate::config::model::HostsFormat;
    use crate::config::resolve::{ConfigId, ResolvedHostsResource};
    use crate::config::{ConfigLoader, LoadOptions};

    use super::{ConfiguredDnsCore, CoreBuildError, DEFAULT_LOCAL_TTL};

    fn example_config() -> Arc<crate::config::ResolvedConfig> {
        ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(include_str!("../../../config-example.yaml"))
            .expect("example config must remain valid")
            .resolved
    }

    #[test]
    fn selects_hosts_core_from_supported_inline_resource() {
        let mut config = example_config();
        Arc::get_mut(&mut config)
            .expect("config fixture must be uniquely owned")
            .hosts
            .retain(|resource| matches!(resource, ResolvedHostsResource::Const { .. }));
        let core = ConfiguredDnsCore::from_config(config.as_ref()).unwrap();

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

        let core = ConfiguredDnsCore::from_config(config).unwrap();
        assert!(!core.has_local_hosts());
    }

    #[test]
    fn upstream_hosts_do_not_become_local_hosts() {
        let mut config = example_config();
        let config = Arc::get_mut(&mut config).expect("config fixture must be uniquely owned");
        config.hosts.clear();

        assert!(config.upstreams.iter().any(|upstream| {
            matches!(
                upstream,
                crate::config::resolve::ResolvedUpstream::Hosts { .. }
            )
        }));

        let core = ConfiguredDnsCore::from_config(config).unwrap();
        assert!(!core.has_local_hosts());
        assert!(matches!(core, ConfiguredDnsCore::ServFail(_)));
    }

    #[test]
    fn loads_json_and_file_hosts_resources() {
        let mut config = example_config();
        let config = Arc::get_mut(&mut config).expect("config fixture must be uniquely owned");
        config.hosts = vec![ResolvedHostsResource::Const {
            id: ConfigId::new("json-hosts").unwrap(),
            format: HostsFormat::Json,
            hosts: r#"{"alias.example":{"CNAME":"target.example"}}"#.to_owned(),
        }];
        assert!(
            ConfiguredDnsCore::from_config(config)
                .unwrap()
                .has_local_hosts()
        );

        let path = std::env::temp_dir().join(format!(
            "fluxdns-configured-hosts-{}-{}.hosts",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock must be after unix epoch")
                .as_nanos()
        ));
        fs::write(&path, "192.0.2.10 file.example\n").unwrap();
        config.hosts = vec![ResolvedHostsResource::File {
            id: ConfigId::new("file-hosts").unwrap(),
            format: HostsFormat::Hosts,
            path: path.clone(),
            auto_update: false,
            update_interval: None,
        }];
        assert!(
            ConfiguredDnsCore::from_config(config)
                .unwrap()
                .has_local_hosts()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reports_file_load_failure_as_structured_core_error() {
        let mut config = example_config();
        let config = Arc::get_mut(&mut config).expect("config fixture must be uniquely owned");
        config.hosts = vec![ResolvedHostsResource::File {
            id: ConfigId::new("missing-hosts").unwrap(),
            format: HostsFormat::Hosts,
            path: std::env::temp_dir().join("fluxdns-missing-hosts-resource"),
            auto_update: false,
            update_interval: None,
        }];

        assert!(matches!(
            ConfiguredDnsCore::from_config(config),
            Err(CoreBuildError::ResourceLoad { resource, .. }) if resource == "missing-hosts"
        ));
    }
}
