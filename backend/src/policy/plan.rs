//! PolicyIndex 到请求级 ResolutionPlan 的纯组合逻辑。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;

use crate::config::resolve::{
    ConfigId, ResolvedClient, ResolvedConfig, ResolvedEcs, ResolvedGlobalCache, ResolvedStrategy,
    ResolvedTtlOverride,
};
use crate::ports::cache::{CacheNamespace, CacheStrategyId, ClientCacheDigest};

use super::{
    ClientIndex, ClientMatch, ClientRuleBuildError, RouteBuildError, RouteIndex, RouteMatch,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyBuildError {
    Client(ClientRuleBuildError),
    Routes(RouteBuildError),
    DuplicateClient(ConfigId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyError {
    RouteNotFound {
        listener: ConfigId,
        path: Option<String>,
    },
    ClientStrategyNotFound {
        client: ConfigId,
        strategy: ConfigId,
    },
    CacheStrategyIdInvalid(ConfigId),
}

#[derive(Clone, Debug)]
pub struct PolicyRequest<'a> {
    pub listener_id: &'a ConfigId,
    pub doh_path: Option<&'a str>,
    pub client_id: Option<&'a str>,
    pub client_addr: Option<IpAddr>,
    pub client_digest: Option<ClientCacheDigest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheDecision {
    Disabled,
    Pool {
        namespace: CacheNamespace,
        optimistic: bool,
    },
}

impl CacheDecision {
    pub fn namespace(&self) -> Option<&CacheNamespace> {
        match self {
            Self::Disabled => None,
            Self::Pool { namespace, .. } => Some(namespace),
        }
    }

    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Pool { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolutionPlan {
    pub listener_id: ConfigId,
    pub route: Option<RouteMatch>,
    pub client: ClientMatch,
    pub strategy: Arc<ResolvedStrategy>,
    pub upstream: ConfigId,
    pub cache: CacheDecision,
    pub ttl_override: ResolvedTtlOverride,
    pub edns_client_subnet: ResolvedEcs,
}

#[derive(Clone, Debug)]
pub struct PolicyIndex {
    routes: RouteIndex,
    clients: ClientIndex,
    client_configs: BTreeMap<ConfigId, Arc<ResolvedClient>>,
    global_cache: ResolvedGlobalCache,
}

impl PolicyIndex {
    pub fn build(
        listeners: impl IntoIterator<Item = crate::config::resolve::ResolvedListener>,
        strategies: impl IntoIterator<Item = ResolvedStrategy>,
        clients: impl IntoIterator<Item = ResolvedClient>,
        global_cache: ResolvedGlobalCache,
    ) -> Result<Self, PolicyBuildError> {
        let routes = RouteIndex::build(listeners, strategies).map_err(PolicyBuildError::Routes)?;
        let clients = clients.into_iter().collect::<Vec<_>>();
        let mut client_configs = BTreeMap::new();
        for client in &clients {
            let id = client.id.clone();
            if client_configs
                .insert(id.clone(), Arc::new(client.clone()))
                .is_some()
            {
                return Err(PolicyBuildError::DuplicateClient(id));
            }
        }
        let client_rules = clients.iter().map(super::ClientRule::from_resolved);
        let clients = ClientIndex::build(client_rules).map_err(PolicyBuildError::Client)?;
        Ok(Self {
            routes,
            clients,
            client_configs,
            global_cache,
        })
    }

    pub fn from_config(config: &ResolvedConfig) -> Result<Self, PolicyBuildError> {
        Self::build(
            config.listeners.clone(),
            config.strategies.clone(),
            config.clients.clone(),
            config.dns.cache.clone(),
        )
    }

    pub fn evaluate(&self, request: PolicyRequest<'_>) -> Result<ResolutionPlan, PolicyError> {
        let routing = if let Some(path) = request.doh_path {
            self.routes
                .select_doh(request.listener_id, path)
                .ok_or_else(|| PolicyError::RouteNotFound {
                    listener: request.listener_id.clone(),
                    path: Some(path.to_owned()),
                })?
        } else {
            self.routes
                .select_stream(request.listener_id)
                .map_err(|_| PolicyError::RouteNotFound {
                    listener: request.listener_id.clone(),
                    path: None,
                })?
        };
        let client = self
            .clients
            .match_client(request.client_id, request.client_addr);
        let client_config = match &client {
            ClientMatch::Matched { client, .. } => self.client_configs.get(&client.name),
            ClientMatch::Unknown => None,
        };
        let strategy = if let Some(client_strategy) =
            client_config.and_then(|client| client.strategy.as_ref())
        {
            self.routes.strategy(client_strategy).ok_or_else(|| {
                PolicyError::ClientStrategyNotFound {
                    client: client_config
                        .expect("client config was checked above")
                        .id
                        .clone(),
                    strategy: client_strategy.clone(),
                }
            })?
        } else {
            routing.strategy
        };
        let cache = self.select_cache(client_config, strategy.as_ref(), request.client_digest)?;
        let ttl_override = client_config
            .map(|client| client.ttl_override.clone())
            .unwrap_or_else(|| strategy.ttl_override.clone());
        let edns_client_subnet = client_config
            .map(|client| client.edns_client_subnet.clone())
            .unwrap_or_else(|| strategy.edns_client_subnet.clone());
        Ok(ResolutionPlan {
            listener_id: routing.listener_id,
            route: routing.route,
            client,
            upstream: strategy.default_upstream.clone(),
            strategy,
            cache,
            ttl_override,
            edns_client_subnet,
        })
    }

    fn select_cache(
        &self,
        client: Option<&Arc<ResolvedClient>>,
        strategy: &ResolvedStrategy,
        digest: Option<ClientCacheDigest>,
    ) -> Result<CacheDecision, PolicyError> {
        if let Some(client) = client
            && let Some(cache) = &client.cache
        {
            if !cache.enabled {
                return Ok(CacheDecision::Disabled);
            }
            let Some(digest) = digest else {
                return Ok(CacheDecision::Disabled);
            };
            return Ok(CacheDecision::Pool {
                namespace: CacheNamespace::ClientStrategy {
                    client_digest: digest,
                    strategy: strategy_namespace(strategy)?,
                },
                optimistic: cache.optimistic.as_ref().is_some_and(|value| value.enabled),
            });
        }
        if let Some(cache) = &strategy.cache {
            if !cache.enabled {
                return Ok(CacheDecision::Disabled);
            }
            return Ok(CacheDecision::Pool {
                namespace: CacheNamespace::Strategy(strategy_namespace(strategy)?),
                optimistic: cache.optimistic.as_ref().is_some_and(|value| value.enabled),
            });
        }
        if !self.global_cache.enabled {
            return Ok(CacheDecision::Disabled);
        }
        Ok(CacheDecision::Pool {
            namespace: CacheNamespace::Global,
            optimistic: self.global_cache.optimistic.enabled,
        })
    }
}

fn strategy_namespace(strategy: &ResolvedStrategy) -> Result<CacheStrategyId, PolicyError> {
    CacheStrategyId::from_validated_config_id(strategy.id.as_str())
        .map_err(|_| PolicyError::CacheStrategyIdInvalid(strategy.id.clone()))
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::path::PathBuf;
    use std::time::Duration;

    use ipnet::IpNet;

    use crate::config::model::EcsMode;
    use crate::config::resolve::{
        ConfigId, ResolvedCacheOverride, ResolvedClient, ResolvedEcs, ResolvedGlobalCache,
        ResolvedListener, ResolvedOptimistic, ResolvedStrategy, ResolvedTtlOverride, ValueSource,
    };
    use crate::ports::cache::ClientCacheDigest;

    use super::{CacheDecision, PolicyIndex, PolicyRequest};

    fn ecs() -> ResolvedEcs {
        ResolvedEcs {
            mode: EcsMode::Disabled,
            custom_ip: None,
            source: ValueSource::Default,
        }
    }

    fn ttl() -> ResolvedTtlOverride {
        ResolvedTtlOverride {
            enabled: false,
            min: None,
            max: None,
            source: ValueSource::Default,
        }
    }

    fn strategy(name: &str, cache: Option<ResolvedCacheOverride>) -> ResolvedStrategy {
        ResolvedStrategy {
            id: ConfigId::new(name).unwrap(),
            rules: Vec::new(),
            default_upstream: ConfigId::new("upstream").unwrap(),
            cache,
            ttl_override: ttl(),
            edns_client_subnet: ecs(),
        }
    }

    fn cache(enabled: bool) -> ResolvedCacheOverride {
        ResolvedCacheOverride {
            enabled,
            optimistic: Some(ResolvedOptimistic {
                enabled: true,
                answer_ttl: Duration::from_secs(10),
                max_age: Duration::from_secs(60),
            }),
            source: ValueSource::Strategy,
        }
    }

    fn client(
        name: &str,
        strategy: Option<&str>,
        cache: Option<ResolvedCacheOverride>,
    ) -> ResolvedClient {
        ResolvedClient {
            id: ConfigId::new(name).unwrap(),
            ids: vec!["alice".to_owned()],
            ips: vec![IpNet::new("192.0.2.0".parse().unwrap(), 24).unwrap()],
            strategy: strategy.map(|value| ConfigId::new(value).unwrap()),
            cache,
            ttl_override: ttl(),
            edns_client_subnet: ecs(),
        }
    }

    fn listener() -> ResolvedListener {
        ResolvedListener::Udp {
            id: ConfigId::new("lan").unwrap(),
            addresses: vec!["127.0.0.1".parse().unwrap()],
            port: 8353,
            strategy: ConfigId::new("default").unwrap(),
            hosts: None,
        }
    }

    fn global(enabled: bool) -> ResolvedGlobalCache {
        ResolvedGlobalCache {
            enabled,
            memory_max_size_bytes: 1024,
            failure_ttl: Duration::from_secs(5),
            optimistic: ResolvedOptimistic {
                enabled: false,
                answer_ttl: Duration::from_secs(10),
                max_age: Duration::from_secs(60),
            },
            persistence_path: PathBuf::from("cache.db"),
            persistence_max_size_bytes: 1024,
        }
    }

    #[test]
    fn client_strategy_override_and_client_cache_are_applied() {
        let index = PolicyIndex::build(
            [listener()],
            [strategy("default", None), strategy("inner", None)],
            [client("client1", Some("inner"), Some(cache(true)))],
            global(true),
        )
        .unwrap();
        let listener_id = ConfigId::new("lan").unwrap();
        let plan = index
            .evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: None,
                client_id: Some("alice"),
                client_addr: Some(IpAddr::from([192, 0, 2, 10])),
                client_digest: Some(ClientCacheDigest::from_digest([0x11; 32])),
            })
            .unwrap();
        assert_eq!(plan.strategy.id.as_str(), "inner");
        assert!(matches!(
            plan.cache,
            CacheDecision::Pool {
                optimistic: true,
                ..
            }
        ));
    }

    #[test]
    fn explicit_disabled_strategy_cache_stops_global_fallback() {
        let index = PolicyIndex::build(
            [listener()],
            [strategy("default", Some(cache(false)))],
            [],
            global(true),
        )
        .unwrap();
        let listener_id = ConfigId::new("lan").unwrap();
        let plan = index
            .evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: None,
                client_id: None,
                client_addr: None,
                client_digest: None,
            })
            .unwrap();
        assert_eq!(plan.cache, CacheDecision::Disabled);
    }

    #[test]
    fn missing_client_digest_does_not_create_client_namespace() {
        let index = PolicyIndex::build(
            [listener()],
            [strategy("default", None)],
            [client("client1", None, Some(cache(true)))],
            global(true),
        )
        .unwrap();
        let listener_id = ConfigId::new("lan").unwrap();
        let plan = index
            .evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: None,
                client_id: Some("alice"),
                client_addr: None,
                client_digest: None,
            })
            .unwrap();
        assert_eq!(plan.cache, CacheDecision::Disabled);
    }
}
