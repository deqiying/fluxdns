//! 已构造 upstream connector 的最小 registry。

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use thiserror::Error;

use crate::config::resolve::{ProxyScheme, ResolvedOutbound, ResolvedUpstream};
use crate::dns::{Cancellation, Deadline};
use crate::ports::exchange::{ConnectorId, DnsExchange};
use crate::ports::{PortError, PortFuture};

use super::{
    BootstrapConnectorRegistry, DohExchange, DohHttpRequest, DohHttpResponseOwned,
    DohHttpTransport, HostsExchange, HostsExchangeBuildError, OutboundProfile,
    ReqwestDohHttpTransport, ReqwestDohHttpTransportBuildError, TokioDohAddressResolver,
    TokioDohHttpTransport, TokioOutboundAddressResolver, TokioOutboundDialer,
    TokioSocks5DohHttpTransport,
};

pub struct UpstreamRegistry {
    connectors: HashMap<ConnectorId, Arc<dyn DnsExchange>>,
}

enum ConfiguredDohTransport {
    Direct(TokioDohHttpTransport),
    Reqwest(ReqwestDohHttpTransport),
    Socks5(TokioSocks5DohHttpTransport<TokioOutboundDialer>),
}

impl DohHttpTransport for ConfiguredDohTransport {
    fn post<'a>(
        &'a self,
        request: DohHttpRequest,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<DohHttpResponseOwned, PortError>> {
        match self {
            Self::Direct(transport) => transport.post(request, deadline, cancellation),
            Self::Reqwest(transport) => transport.post(request, deadline, cancellation),
            Self::Socks5(transport) => transport.post(request, deadline, cancellation),
        }
    }
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
    #[error("upstream `{upstream}` could not build a DoH connector")]
    InvalidDoh { upstream: String },
    #[error("upstream `{upstream}` could not build its HTTP transport")]
    InvalidDohTransport { upstream: String },
    #[error("outbound `{outbound}` could not build a proxy profile: {source}")]
    InvalidOutbound {
        outbound: String,
        source: super::OutboundProfileError,
    },
    #[error("outbound `{outbound}` is duplicated")]
    DuplicateOutbound { outbound: String },
    #[error("upstream `{upstream}` references missing outbound `{outbound}`")]
    MissingOutbound { upstream: String, outbound: String },
    #[error("upstream `{upstream}` cannot use bootstrap with outbound `{outbound}`")]
    InvalidOutboundCombination { upstream: String, outbound: String },
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
        let bootstrap_registry = Arc::new(BootstrapConnectorRegistry::default());
        let resolver = Arc::new(TokioDohAddressResolver::with_bootstrap_registry(
            bootstrap_registry.clone(),
        ));
        let transport = Arc::new(TokioDohHttpTransport::with_resolver(resolver));
        Self::from_resolved_with_doh_transport_and_bootstrap_registry(
            upstreams,
            transport,
            Some(bootstrap_registry),
        )
    }

    pub fn from_resolved_with_outbounds(
        upstreams: &[ResolvedUpstream],
        outbounds: &[ResolvedOutbound],
    ) -> Result<Self, RegistryError> {
        let bootstrap_registry = Arc::new(BootstrapConnectorRegistry::default());
        let target_resolver = Arc::new(TokioDohAddressResolver::with_bootstrap_registry(
            bootstrap_registry.clone(),
        ));
        let proxy_resolver = Arc::new(TokioOutboundAddressResolver::new());
        let dialer = Arc::new(TokioOutboundDialer::new());
        let mut profiles = std::collections::HashMap::new();
        for outbound in outbounds {
            if profiles.contains_key(&outbound.id) {
                return Err(RegistryError::DuplicateOutbound {
                    outbound: outbound.id.as_str().to_owned(),
                });
            }
            let profile =
                OutboundProfile::from_resolved(outbound, 64 * 1024).map_err(|source| {
                    RegistryError::InvalidOutbound {
                        outbound: outbound.id.as_str().to_owned(),
                        source,
                    }
                })?;
            profiles.insert(outbound.id.clone(), profile);
        }

        let transport_factory =
            |upstream: &ResolvedUpstream, _id: &crate::config::resolve::ConfigId| {
                let ResolvedUpstream::Doh {
                    id,
                    address,
                    proxy,
                    bootstrap,
                    ..
                } = upstream
                else {
                    return Ok(Arc::new(ConfiguredDohTransport::Direct(
                        TokioDohHttpTransport::with_resolver(target_resolver.clone()),
                    )));
                };
                if address.scheme() == "https" {
                    let transport = if let Some(proxy_id) = proxy {
                        let Some(profile) = profiles.get(proxy_id) else {
                            return Err(RegistryError::MissingOutbound {
                                upstream: id.as_str().to_owned(),
                                outbound: proxy_id.as_str().to_owned(),
                            });
                        };
                        if matches!(profile.scheme(), ProxyScheme::Socks5h) && bootstrap.is_some() {
                            return Err(RegistryError::InvalidOutboundCombination {
                                upstream: id.as_str().to_owned(),
                                outbound: proxy_id.as_str().to_owned(),
                            });
                        }
                        ReqwestDohHttpTransport::with_proxy(
                            target_resolver.clone(),
                            proxy_resolver.clone(),
                            profile.clone(),
                        )
                    } else {
                        ReqwestDohHttpTransport::new(target_resolver.clone())
                    }
                    .map_err(|_error: ReqwestDohHttpTransportBuildError| {
                        RegistryError::InvalidDohTransport {
                            upstream: id.as_str().to_owned(),
                        }
                    })?;
                    return Ok(Arc::new(ConfiguredDohTransport::Reqwest(transport)));
                }
                let Some(proxy_id) = proxy else {
                    return Ok(Arc::new(ConfiguredDohTransport::Direct(
                        TokioDohHttpTransport::with_resolver(target_resolver.clone()),
                    )));
                };
                let Some(profile) = profiles.get(proxy_id) else {
                    return Err(RegistryError::MissingOutbound {
                        upstream: id.as_str().to_owned(),
                        outbound: proxy_id.as_str().to_owned(),
                    });
                };
                if matches!(profile.scheme(), ProxyScheme::Socks5h) && bootstrap.is_some() {
                    return Err(RegistryError::InvalidOutboundCombination {
                        upstream: id.as_str().to_owned(),
                        outbound: proxy_id.as_str().to_owned(),
                    });
                }
                Ok(Arc::new(ConfiguredDohTransport::Socks5(
                    TokioSocks5DohHttpTransport::new(
                        profile.clone(),
                        dialer.clone(),
                        proxy_resolver.clone(),
                        target_resolver.clone(),
                    ),
                )))
            };

        Self::build_connectors(upstreams, Some(bootstrap_registry), transport_factory)
    }

    pub fn from_resolved_with_doh_transport<T>(
        upstreams: &[ResolvedUpstream],
        doh_transport: Arc<T>,
    ) -> Result<Self, RegistryError>
    where
        T: DohHttpTransport + 'static,
    {
        Self::from_resolved_with_doh_transport_and_bootstrap_registry(
            upstreams,
            doh_transport,
            None,
        )
    }

    fn from_resolved_with_doh_transport_and_bootstrap_registry<T>(
        upstreams: &[ResolvedUpstream],
        doh_transport: Arc<T>,
        bootstrap_registry: Option<Arc<BootstrapConnectorRegistry>>,
    ) -> Result<Self, RegistryError>
    where
        T: DohHttpTransport + 'static,
    {
        Self::build_connectors(upstreams, bootstrap_registry, |upstream, id| {
            if matches!(upstream, ResolvedUpstream::Doh { proxy: Some(_), .. }) {
                return Err(RegistryError::UnsupportedUpstream {
                    upstream: id.as_str().to_owned(),
                    kind: "doh_proxy",
                });
            }
            if matches!(upstream, ResolvedUpstream::Doh { address, .. } if address.scheme() == "https")
            {
                return Err(RegistryError::UnsupportedUpstream {
                    upstream: id.as_str().to_owned(),
                    kind: "doh_https",
                });
            }
            Ok(doh_transport.clone())
        })
    }

    fn build_connectors<T, F>(
        upstreams: &[ResolvedUpstream],
        bootstrap_registry: Option<Arc<BootstrapConnectorRegistry>>,
        mut transport_factory: F,
    ) -> Result<Self, RegistryError>
    where
        T: DohHttpTransport + 'static,
        F: FnMut(
            &ResolvedUpstream,
            &crate::config::resolve::ConfigId,
        ) -> Result<Arc<T>, RegistryError>,
    {
        let mut connectors = HashMap::new();
        for upstream in upstreams {
            let id = match upstream {
                ResolvedUpstream::Hosts { id, .. }
                | ResolvedUpstream::Doh { id, .. }
                | ResolvedUpstream::Group { id, .. } => id,
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
            let exchange: Arc<dyn DnsExchange> = match upstream {
                ResolvedUpstream::Hosts { .. } => {
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
                    Arc::new(exchange)
                }
                ResolvedUpstream::Doh {
                    address,
                    bootstrap,
                    connect_ip,
                    ..
                } => {
                    if !matches!(address.scheme(), "http" | "https") {
                        return Err(RegistryError::InvalidDoh {
                            upstream: id.as_str().to_owned(),
                        });
                    }
                    let doh_transport = transport_factory(upstream, id)?;
                    let exchange = DohExchange::new_with_bootstrap(
                        connector.clone(),
                        address.clone(),
                        *connect_ip,
                        bootstrap.clone(),
                        doh_transport.clone(),
                    )
                    .map_err(|_| RegistryError::InvalidDoh {
                        upstream: id.as_str().to_owned(),
                    })?;
                    Arc::new(exchange)
                }
                ResolvedUpstream::Group { .. } => {
                    return Err(RegistryError::UnsupportedUpstream {
                        upstream: id.as_str().to_owned(),
                        kind: "group",
                    });
                }
            };
            if let Some(bootstrap_registry) = bootstrap_registry.as_ref() {
                bootstrap_registry.insert(id.clone(), exchange.clone());
            }
            connectors.insert(connector, exchange);
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
    use std::fs;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::config::model::EcsMode;
    use crate::config::resolve::{
        ConfigId, ResolvedEcs, ResolvedOutbound, ResolvedSecretRef, ResolvedUpstream, ValueSource,
    };
    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, ClientIdentity, Deadline, ListenerId,
        RequestContext, RequestId, RequestMeta, RuntimeRevision, TransportCapabilities,
        TransportClass,
    };
    use crate::ports::exchange::UpstreamOutcome;

    use super::{RegistryError, UpstreamRegistry};

    fn hosts(name: &str) -> ResolvedUpstream {
        ResolvedUpstream::Hosts {
            id: ConfigId::new(name.to_owned()).unwrap(),
            format: "hosts".to_owned(),
            hosts: "192.0.2.1 example.test\n".to_owned(),
        }
    }

    fn outbound(url: &str) -> (ResolvedOutbound, std::path::PathBuf) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "fluxdns-registry-outbound-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("proxy-url");
        fs::write(&path, url).unwrap();
        (
            ResolvedOutbound {
                id: ConfigId::new("socks").unwrap(),
                kind: crate::config::model::OutboundType::Socks5,
                proxy_url: ResolvedSecretRef {
                    env: None,
                    file: Some(path),
                },
            },
            root,
        )
    }

    fn context() -> RequestContext {
        let now = Instant::now();
        RequestContext {
            meta: RequestMeta {
                request_id: RequestId(1),
                trace_id: None,
                received_at: now,
                received_at_utc: SystemTime::now(),
                deadline: Deadline::new(now + Duration::from_secs(5)),
                cancellation: Cancellation::new(),
                connection_id: None,
                stream_id: None,
                listener_id: ListenerId::from("registry-test"),
                route_id: None,
                original_dns_id: Some(7),
            },
            client: ClientIdentity::default(),
            transport: TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            runtime_revision: RuntimeRevision(1),
        }
    }

    fn query() -> CanonicalQuery {
        let mut message = Message::new(7, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("service.example.").unwrap(),
            RecordType::A,
        ));
        CanonicalQuery::from_message(message).unwrap()
    }

    async fn read_http_body(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = std::str::from_utf8(&bytes[..header_end - 4]).unwrap();
        let content_length = headers
            .split("\r\n")
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() < header_end + content_length {
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        bytes[header_end..header_end + content_length].to_vec()
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
        let https = ResolvedUpstream::Doh {
            id: ConfigId::new("remote").unwrap(),
            address: "https://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: None,
            proxy: None,
            edns_client_subnet: None,
        };

        assert!(matches!(
            UpstreamRegistry::from_resolved(&[https]),
            Err(RegistryError::UnsupportedUpstream { upstream, kind })
                if upstream == "remote" && kind == "doh_https"
        ));

        let proxy = ResolvedUpstream::Doh {
            id: ConfigId::new("proxy").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: None,
            proxy: Some(ConfigId::new("socks").unwrap()),
            edns_client_subnet: None,
        };
        assert!(matches!(
            UpstreamRegistry::from_resolved(&[proxy]),
            Err(RegistryError::UnsupportedUpstream { upstream, kind })
                if upstream == "proxy" && kind == "doh_proxy"
        ));
    }

    #[test]
    fn explicit_upstream_ecs_does_not_block_connector_build() {
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("ecs").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: None,
            proxy: None,
            edns_client_subnet: Some(ResolvedEcs {
                mode: EcsMode::Client,
                custom_ip: None,
                source: ValueSource::Upstream,
            }),
        };

        assert!(UpstreamRegistry::from_resolved(&[doh]).is_ok());
    }

    #[test]
    fn inherited_global_ecs_does_not_block_connector_build() {
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("remote").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: Some("192.0.2.44".parse().unwrap()),
            proxy: None,
            edns_client_subnet: Some(ResolvedEcs {
                mode: EcsMode::Custom,
                custom_ip: Some("203.0.113.0/24".parse().unwrap()),
                source: ValueSource::Global,
            }),
        };

        assert!(UpstreamRegistry::from_resolved(&[doh]).is_ok());
    }

    #[test]
    fn builds_plain_http_doh_connector() {
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("remote").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: Some("192.0.2.44".parse().unwrap()),
            proxy: None,
            edns_client_subnet: None,
        };

        let registry = UpstreamRegistry::from_resolved(&[doh]).unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry
                .get_by_name("remote")
                .unwrap()
                .connector_id()
                .as_str(),
            "remote"
        );
    }

    #[test]
    fn config_aware_registry_builds_https_doh_connector() {
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("secure").unwrap(),
            address: "https://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: None,
            proxy: None,
            edns_client_subnet: None,
        };

        let registry = UpstreamRegistry::from_resolved_with_outbounds(&[doh], &[]).unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry
                .get_by_name("secure")
                .unwrap()
                .connector_id()
                .as_str(),
            "secure"
        );
    }

    #[test]
    fn config_aware_registry_builds_https_doh_connector_with_socks_proxy() {
        let (outbound, root) = outbound("socks5://127.0.0.1:1080");
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("secure-proxy").unwrap(),
            address: "https://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: None,
            proxy: Some(ConfigId::new("socks").unwrap()),
            edns_client_subnet: None,
        };

        let registry = UpstreamRegistry::from_resolved_with_outbounds(&[doh], &[outbound]).unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry
                .get_by_name("secure-proxy")
                .unwrap()
                .connector_id()
                .as_str(),
            "secure-proxy"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_aware_registry_rejects_https_socks5h_bootstrap_combination() {
        let (outbound, root) = outbound("socks5h://127.0.0.1:1080");
        let doh = ResolvedUpstream::Doh {
            id: ConfigId::new("secure-bootstrap").unwrap(),
            address: "https://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: Some(ConfigId::new("bootstrap").unwrap()),
            connect_ip: None,
            proxy: Some(ConfigId::new("socks").unwrap()),
            edns_client_subnet: None,
        };

        assert!(matches!(
            UpstreamRegistry::from_resolved_with_outbounds(&[doh], &[outbound]),
            Err(RegistryError::InvalidOutboundCombination { upstream, outbound })
                if upstream == "secure-bootstrap" && outbound == "socks"
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_aware_registry_rejects_missing_or_invalid_outbound_profiles() {
        let upstream = ResolvedUpstream::Doh {
            id: ConfigId::new("remote").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: None,
            connect_ip: Some("192.0.2.44".parse().unwrap()),
            proxy: Some(ConfigId::new("socks").unwrap()),
            edns_client_subnet: None,
        };
        assert!(matches!(
            UpstreamRegistry::from_resolved_with_outbounds(std::slice::from_ref(&upstream), &[]),
            Err(RegistryError::MissingOutbound { upstream, outbound })
                if upstream == "remote" && outbound == "socks"
        ));

        let (invalid, root) = outbound("https://not-socks.example");
        assert!(matches!(
            UpstreamRegistry::from_resolved_with_outbounds(&[upstream], &[invalid]),
            Err(RegistryError::InvalidOutbound { outbound, .. }) if outbound == "socks"
        ));
        fs::remove_dir_all(root).unwrap();

        let (socks5h, root) = outbound("socks5h://127.0.0.1");
        let bootstrap_upstream = ResolvedUpstream::Doh {
            id: ConfigId::new("remote-bootstrap").unwrap(),
            address: "http://dns.example.test/dns-query".parse().unwrap(),
            bootstrap: Some(ConfigId::new("bootstrap").unwrap()),
            connect_ip: None,
            proxy: Some(ConfigId::new("socks").unwrap()),
            edns_client_subnet: None,
        };
        assert!(matches!(
            UpstreamRegistry::from_resolved_with_outbounds(
                &[bootstrap_upstream],
                &[socks5h]
            ),
            Err(RegistryError::InvalidOutboundCombination { upstream, outbound })
                if upstream == "remote-bootstrap" && outbound == "socks"
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn default_registry_executes_doh_bootstrap_through_hosts_connector() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = read_http_body(&mut stream).await;
            let request = Message::from_vec(&body).unwrap();
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

        let upstreams = vec![
            ResolvedUpstream::Hosts {
                id: ConfigId::new("bootstrap").unwrap(),
                format: "hosts".to_owned(),
                hosts: "127.0.0.1 resolver.example.test\n".to_owned(),
            },
            ResolvedUpstream::Doh {
                id: ConfigId::new("remote").unwrap(),
                address: format!("http://resolver.example.test:{port}/dns-query")
                    .parse()
                    .unwrap(),
                bootstrap: Some(ConfigId::new("bootstrap").unwrap()),
                connect_ip: None,
                proxy: None,
                edns_client_subnet: None,
            },
        ];
        let registry = UpstreamRegistry::from_resolved(&upstreams).unwrap();
        let connector = registry.get_by_name("remote").unwrap();

        assert!(matches!(
            connector.exchange(&query(), &context()).await,
            UpstreamOutcome::Response(response)
                if response.class() == crate::dns::ResponseClass::NoData
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn config_aware_registry_executes_plain_http_doh_through_socks5_and_bootstrap() {
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
            assert_eq!(connect, [5, 1, 0, 1, 127, 0, 0, 1, 0, 80]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();

            let body = read_http_body(&mut stream).await;
            let request = Message::from_vec(&body).unwrap();
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

        let (outbound, root) = outbound(&format!("socks5://127.0.0.1:{proxy_port}"));
        let upstreams = vec![
            ResolvedUpstream::Hosts {
                id: ConfigId::new("bootstrap").unwrap(),
                format: "hosts".to_owned(),
                hosts: "127.0.0.1 dns.example.test\n".to_owned(),
            },
            ResolvedUpstream::Doh {
                id: ConfigId::new("remote").unwrap(),
                address: "http://dns.example.test:80/dns-query".parse().unwrap(),
                bootstrap: Some(ConfigId::new("bootstrap").unwrap()),
                connect_ip: None,
                proxy: Some(ConfigId::new("socks").unwrap()),
                edns_client_subnet: None,
            },
        ];
        let registry =
            UpstreamRegistry::from_resolved_with_outbounds(&upstreams, &[outbound]).unwrap();
        let connector = registry.get_by_name("remote").unwrap();

        assert!(matches!(
            connector.exchange(&query(), &context()).await,
            UpstreamOutcome::Response(response)
                if response.class() == crate::dns::ResponseClass::NoData
        ));
        server.await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
