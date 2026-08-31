//! 已构造 upstream connector 的最小 registry。

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use thiserror::Error;

use crate::config::resolve::ResolvedUpstream;
use crate::ports::exchange::{ConnectorId, DnsExchange};

use super::{HostsExchange, HostsExchangeBuildError};

pub struct UpstreamRegistry {
    connectors: HashMap<ConnectorId, Arc<dyn DnsExchange>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum RegistryError {
    #[error("upstream `{upstream}` has an invalid connector id")]
    InvalidConnectorId { upstream: String },
    #[error("connector `{connector}` is duplicated")]
    DuplicateConnector { connector: String },
    #[error("connector `{connector}` is not registered")]
    MissingConnector { connector: String },
    #[error("upstream `{upstream}` could not build a hosts connector")]
    InvalidHosts { upstream: String },
    #[error("upstream `{upstream}` has unsupported connector type `{kind}`")]
    UnsupportedUpstream {
        upstream: String,
        kind: &'static str,
    },
}

impl fmt::Debug for UpstreamRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamRegistry")
            .field("connector_count", &self.connectors.len())
            .finish()
    }
}

impl UpstreamRegistry {
    pub fn from_resolved(upstreams: &[ResolvedUpstream]) -> Result<Self, RegistryError> {
        let mut connectors = HashMap::new();
        for upstream in upstreams {
            let ResolvedUpstream::Hosts { id, .. } = upstream else {
                let (id, kind) = match upstream {
                    ResolvedUpstream::Doh { id, .. } => (id, "doh"),
                    ResolvedUpstream::Group { id, .. } => (id, "group"),
                    ResolvedUpstream::Hosts { .. } => unreachable!("hosts matched above"),
                };
                return Err(RegistryError::UnsupportedUpstream {
                    upstream: id.as_str().to_owned(),
                    kind,
                });
            };
            let connector = ConnectorId::new(id.as_str().to_owned()).map_err(|_| {
                RegistryError::InvalidConnectorId {
                    upstream: id.as_str().to_owned(),
                }
            })?;
            if connectors.contains_key(&connector) {
                return Err(RegistryError::DuplicateConnector {
                    connector: connector.as_str().to_owned(),
                });
            }
            let exchange = HostsExchange::from_resolved(upstream).map_err(|error| {
                let upstream = match error {
                    HostsExchangeBuildError::NotHosts { upstream }
                    | HostsExchangeBuildError::UnsupportedFormat { upstream, .. }
                    | HostsExchangeBuildError::InvalidData { upstream }
                    | HostsExchangeBuildError::UnsupportedRecordType { upstream, .. }
                    | HostsExchangeBuildError::EmptyAddressList { upstream } => upstream,
                };
                RegistryError::InvalidHosts { upstream }
            })?;
            connectors.insert(connector, Arc::new(exchange) as Arc<dyn DnsExchange>);
        }
        Ok(Self { connectors })
    }

    pub fn get(&self, connector: &ConnectorId) -> Result<Arc<dyn DnsExchange>, RegistryError> {
        self.connectors
            .get(connector)
            .cloned()
            .ok_or_else(|| RegistryError::MissingConnector {
                connector: connector.as_str().to_owned(),
            })
    }

    pub fn get_by_name(&self, name: &str) -> Result<Arc<dyn DnsExchange>, RegistryError> {
        let connector =
            ConnectorId::new(name.to_owned()).map_err(|_| RegistryError::InvalidConnectorId {
                upstream: name.to_owned(),
            })?;
        self.get(&connector)
    }

    pub fn len(&self) -> usize {
        self.connectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.connectors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::config::resolve::{ConfigId, ResolvedUpstream};

    use super::{RegistryError, UpstreamRegistry};

    fn hosts(name: &str) -> ResolvedUpstream {
        ResolvedUpstream::Hosts {
            id: ConfigId::new(name.to_owned()).unwrap(),
            format: "hosts".to_owned(),
            hosts: "192.0.2.1 example.test\n".to_owned(),
        }
    }

    #[test]
    fn builds_hosts_connectors_and_reports_missing_connector() {
        let registry = UpstreamRegistry::from_resolved(&[hosts("local")]).unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry
                .get_by_name("local")
                .unwrap()
                .connector_id()
                .as_str(),
            "local"
        );
        assert!(matches!(
            registry.get_by_name("missing"),
            Err(RegistryError::MissingConnector { connector }) if connector == "missing"
        ));
    }

    #[test]
    fn rejects_duplicate_and_invalid_connector_ids() {
        assert!(matches!(
            UpstreamRegistry::from_resolved(&[hosts("local"), hosts("local")]),
            Err(RegistryError::DuplicateConnector { connector }) if connector == "local"
        ));

        let invalid = ResolvedUpstream::Hosts {
            id: ConfigId::new("invalid!".to_owned()).unwrap(),
            format: "hosts".to_owned(),
            hosts: "192.0.2.1 example.test\n".to_owned(),
        };
        assert!(matches!(
            UpstreamRegistry::from_resolved(&[invalid]),
            Err(RegistryError::InvalidConnectorId { upstream }) if upstream == "invalid!"
        ));
    }

    #[test]
    fn rejects_unimplemented_connector_types_at_build_boundary() {
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("remote").unwrap(),
            address: "https://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: None,
            proxy: None,
            edns_client_subnet: None,
        };

        assert!(matches!(
            UpstreamRegistry::from_resolved(&[doh]),
            Err(RegistryError::UnsupportedUpstream { upstream, kind })
                if upstream == "remote" && kind == "doh"
        ));
    }
}
