//! PolicyIndex 到请求级 ResolutionPlan 的纯组合逻辑。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;

use crate::config::model::RuleSetFormat;
use crate::config::resolve::{
    ConfigId, ResolvedClient, ResolvedConfig, ResolvedEcs, ResolvedGlobalCache,
    ResolvedHostsResource, ResolvedRuleSet, ResolvedStrategy, ResolvedTtlOverride,
    ResolvedUpstream, ValueSource,
};
use crate::ports::cache::{CacheNamespace, CacheStrategyId, ClientCacheDigest};
use crate::resource::{
    CanonicalDomain, HostsIndex, HostsLimits, HostsParseError, ResourceLoadError, RuleIndex,
    RuleLimits, RuleMatch, RuleParseError, RuleResourceLoadError, load_hosts, load_rule_set,
};

use super::{
    ClientIndex, ClientMatch, ClientRuleBuildError, RouteBuildError, RouteIndex, RouteMatch,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyBuildError {
    Client(ClientRuleBuildError),
    Routes(RouteBuildError),
    DuplicateClient(ConfigId),
    DuplicateHostsResource(ConfigId),
    DuplicateRuleSet(ConfigId),
    HostsParse {
        resource: ConfigId,
        source: HostsParseError,
    },
    RuleSetParse {
        resource: ConfigId,
        source: RuleParseError,
    },
    HostsLoad {
        resource: ConfigId,
        source: ResourceLoadError,
    },
    RuleSetLoad {
        resource: ConfigId,
        source: RuleResourceLoadError,
    },
    UnsupportedHostsResource {
        resource: ConfigId,
        kind: &'static str,
    },
    UnsupportedRuleSet {
        resource: ConfigId,
        kind: &'static str,
    },
    UnsupportedRuleSetSelector {
        resource: ConfigId,
        selector: String,
    },
}
type ResourceIndexes = (
    BTreeMap<ConfigId, Arc<HostsIndex>>,
    BTreeMap<ConfigId, Arc<RuleIndex>>,
);

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
    HostsResourceNotFound(ConfigId),
    RuleSetNotFound(ConfigId),
    UnsupportedRuleSetSelector {
        resource: ConfigId,
        selector: String,
    },
}

#[derive(Clone, Debug)]
pub struct PolicyRequest<'a> {
    pub listener_id: &'a ConfigId,
    pub doh_path: Option<&'a str>,
    pub client_id: Option<&'a str>,
    pub client_addr: Option<IpAddr>,
    pub client_digest: Option<ClientCacheDigest>,
    pub qname: Option<&'a CanonicalDomain>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MatchedRuleKind {
    ListenerHosts {
        resource: ConfigId,
    },
    Hosts {
        resource: ConfigId,
    },
    RuleSet {
        resource: ConfigId,
        matcher: RuleMatch,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchedRule {
    pub strategy: ConfigId,
    pub ordinal: usize,
    pub kind: MatchedRuleKind,
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
    pub hosts: Option<ConfigId>,
    pub matched_rule: Option<MatchedRule>,
}

#[derive(Clone, Debug)]
pub struct PolicyIndex {
    routes: RouteIndex,
    clients: ClientIndex,
    client_configs: BTreeMap<ConfigId, Arc<ResolvedClient>>,
    upstream_ecs: BTreeMap<ConfigId, ResolvedEcs>,
    global_cache: ResolvedGlobalCache,
    hosts: BTreeMap<ConfigId, Arc<HostsIndex>>,
    rule_sets: BTreeMap<ConfigId, Arc<RuleIndex>>,
}

impl PolicyIndex {
    pub fn build(
        listeners: impl IntoIterator<Item = crate::config::resolve::ResolvedListener>,
        strategies: impl IntoIterator<Item = ResolvedStrategy>,
        clients: impl IntoIterator<Item = ResolvedClient>,
        global_cache: ResolvedGlobalCache,
    ) -> Result<Self, PolicyBuildError> {
        Self::build_with_resources(
            listeners,
            strategies,
            clients,
            BTreeMap::new(),
            global_cache,
            BTreeMap::new(),
            BTreeMap::new(),
        )
    }

    fn build_with_resources(
        listeners: impl IntoIterator<Item = crate::config::resolve::ResolvedListener>,
        strategies: impl IntoIterator<Item = ResolvedStrategy>,
        clients: impl IntoIterator<Item = ResolvedClient>,
        upstream_ecs: BTreeMap<ConfigId, ResolvedEcs>,
        global_cache: ResolvedGlobalCache,
        hosts: BTreeMap<ConfigId, Arc<HostsIndex>>,
        rule_sets: BTreeMap<ConfigId, Arc<RuleIndex>>,
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
            upstream_ecs,
            global_cache,
            hosts,
            rule_sets,
        })
    }

    pub fn from_config(config: &ResolvedConfig) -> Result<Self, PolicyBuildError> {
        let (hosts, rule_sets) =
            compile_resources(&config.hosts, &config.rule_sets, &BTreeMap::new())?;
        Self::build_with_resources(
            config.listeners.clone(),
            config.strategies.clone(),
            config.clients.clone(),
            collect_upstream_ecs(&config.upstreams),
            config.dns.cache.clone(),
            hosts,
            rule_sets,
        )
    }

    pub(crate) fn from_config_with_resource_indexes(
        config: &ResolvedConfig,
        supplied_hosts: &BTreeMap<ConfigId, Arc<HostsIndex>>,
        supplied_rule_indexes: &BTreeMap<ConfigId, Arc<RuleIndex>>,
    ) -> Result<Self, PolicyBuildError> {
        let (hosts, rule_sets) = compile_resources_with_supplied(
            &config.hosts,
            &config.rule_sets,
            supplied_hosts,
            supplied_rule_indexes,
        )?;
        Self::build_with_resources(
            config.listeners.clone(),
            config.strategies.clone(),
            config.clients.clone(),
            collect_upstream_ecs(&config.upstreams),
            config.dns.cache.clone(),
            hosts,
            rule_sets,
        )
    }

    pub(crate) fn host_resource_count(&self) -> usize {
        self.hosts.len()
    }

    pub(crate) fn hosts_index(&self, resource: &ConfigId) -> Option<Arc<HostsIndex>> {
        self.hosts.get(resource).cloned()
    }

    pub(crate) fn replace_hosts_resource(
        &self,
        resource: &ConfigId,
        index: HostsIndex,
    ) -> Result<Self, PolicyError> {
        if !self.hosts.contains_key(resource) {
            return Err(PolicyError::HostsResourceNotFound(resource.clone()));
        }
        let mut next = self.clone();
        next.hosts.insert(resource.clone(), Arc::new(index));
        Ok(next)
    }

    pub(crate) fn replace_rule_set_resource(
        &self,
        resource: &ConfigId,
        index: RuleIndex,
    ) -> Result<Self, PolicyError> {
        if !self.rule_sets.contains_key(resource) {
            return Err(PolicyError::RuleSetNotFound(resource.clone()));
        }
        let mut next = self.clone();
        next.rule_sets.insert(resource.clone(), Arc::new(index));
        Ok(next)
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
            routing.strategy.clone()
        };
        let cache = self.select_cache(client_config, strategy.as_ref(), request.client_digest)?;
        let ttl_override = client_config
            .filter(|client| client.ttl_override.source == ValueSource::Client)
            .map(|client| client.ttl_override.clone())
            .unwrap_or_else(|| strategy.ttl_override.clone());
        let (hosts, matched_rule, upstream, strategy_ecs) = self.evaluate_rules(
            &routing,
            strategy.as_ref(),
            request.qname,
            strategy.edns_client_subnet.clone(),
        )?;
        let edns_client_subnet = select_ecs(
            strategy_ecs,
            client_config,
            self.upstream_ecs.get(&upstream),
        );
        Ok(ResolutionPlan {
            listener_id: routing.listener_id,
            route: routing.route,
            client,
            upstream,
            strategy,
            cache,
            ttl_override,
            edns_client_subnet,
            hosts,
            matched_rule,
        })
    }

    fn evaluate_rules(
        &self,
        routing: &super::RouteSelection,
        strategy: &ResolvedStrategy,
        qname: Option<&CanonicalDomain>,
        inherited_ecs: ResolvedEcs,
    ) -> Result<(Option<ConfigId>, Option<MatchedRule>, ConfigId, ResolvedEcs), PolicyError> {
        let default = || {
            (
                None,
                None,
                strategy.default_upstream.clone(),
                inherited_ecs.clone(),
            )
        };
        let Some(qname) = qname else {
            return Ok(default());
        };

        if let Some(resource) = &routing.listener_hosts {
            let index = self
                .hosts
                .get(resource)
                .ok_or_else(|| PolicyError::HostsResourceNotFound(resource.clone()))?;
            if index.lookup(qname).is_some() {
                return Ok((
                    Some(resource.clone()),
                    Some(MatchedRule {
                        strategy: strategy.id.clone(),
                        ordinal: usize::MAX,
                        kind: MatchedRuleKind::ListenerHosts {
                            resource: resource.clone(),
                        },
                    }),
                    strategy.default_upstream.clone(),
                    inherited_ecs.clone(),
                ));
            }
        }

        for (ordinal, rule) in strategy.rules.iter().enumerate() {
            if let Some(resource) = &rule.hosts {
                let index = self
                    .hosts
                    .get(resource)
                    .ok_or_else(|| PolicyError::HostsResourceNotFound(resource.clone()))?;
                if index.lookup(qname).is_some() {
                    return Ok((
                        Some(resource.clone()),
                        Some(MatchedRule {
                            strategy: strategy.id.clone(),
                            ordinal,
                            kind: MatchedRuleKind::Hosts {
                                resource: resource.clone(),
                            },
                        }),
                        strategy.default_upstream.clone(),
                        rule.edns_client_subnet.clone(),
                    ));
                }
                continue;
            }

            let Some(rule_set) = &rule.rule_set else {
                continue;
            };
            if let Some(selector) = &rule_set.selector {
                return Err(PolicyError::UnsupportedRuleSetSelector {
                    resource: rule_set.resource.clone(),
                    selector: selector.clone(),
                });
            }
            let index = self
                .rule_sets
                .get(&rule_set.resource)
                .ok_or_else(|| PolicyError::RuleSetNotFound(rule_set.resource.clone()))?;
            if let Some(matcher) = index.matches(qname) {
                return Ok((
                    None,
                    Some(MatchedRule {
                        strategy: strategy.id.clone(),
                        ordinal,
                        kind: MatchedRuleKind::RuleSet {
                            resource: rule_set.resource.clone(),
                            matcher,
                        },
                    }),
                    rule.upstream
                        .clone()
                        .unwrap_or_else(|| strategy.default_upstream.clone()),
                    rule.edns_client_subnet.clone(),
                ));
            }
        }
        Ok(default())
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

/// 收集 direct DoH upstream 的 ECS 基线，供请求选出最终 upstream 后补齐优先级。
fn collect_upstream_ecs(upstreams: &[ResolvedUpstream]) -> BTreeMap<ConfigId, ResolvedEcs> {
    upstreams
        .iter()
        .filter_map(|upstream| match upstream {
            ResolvedUpstream::Doh {
                id,
                edns_client_subnet: Some(ecs),
                ..
            } => Some((id.clone(), ecs.clone())),
            ResolvedUpstream::Hosts { .. }
            | ResolvedUpstream::Doh {
                edns_client_subnet: None,
                ..
            }
            | ResolvedUpstream::Group { .. } => None,
        })
        .collect()
}

/// 按 rule/strategy、client、upstream、global 的顺序选出最终 ECS 配置。
fn select_ecs(
    strategy_ecs: ResolvedEcs,
    client: Option<&Arc<ResolvedClient>>,
    upstream_ecs: Option<&ResolvedEcs>,
) -> ResolvedEcs {
    if matches!(
        strategy_ecs.source,
        ValueSource::Rule | ValueSource::Strategy
    ) {
        return strategy_ecs;
    }
    if let Some(client_ecs) = client
        .map(|client| &client.edns_client_subnet)
        .filter(|ecs| ecs.source == ValueSource::Client)
    {
        return client_ecs.clone();
    }
    upstream_ecs.cloned().unwrap_or(strategy_ecs)
}

fn compile_resources(
    hosts: &[ResolvedHostsResource],
    rule_sets: &[ResolvedRuleSet],
    remote_rule_indexes: &BTreeMap<ConfigId, Arc<RuleIndex>>,
) -> Result<ResourceIndexes, PolicyBuildError> {
    compile_resources_with_supplied(hosts, rule_sets, &BTreeMap::new(), remote_rule_indexes)
}

fn compile_resources_with_supplied(
    hosts: &[ResolvedHostsResource],
    rule_sets: &[ResolvedRuleSet],
    supplied_hosts: &BTreeMap<ConfigId, Arc<HostsIndex>>,
    supplied_rule_indexes: &BTreeMap<ConfigId, Arc<RuleIndex>>,
) -> Result<ResourceIndexes, PolicyBuildError> {
    let mut host_indexes = BTreeMap::new();
    for resource in hosts {
        let id = match resource {
            ResolvedHostsResource::Const { id, .. } | ResolvedHostsResource::File { id, .. } => id,
        };
        let index = if let Some(index) = supplied_hosts.get(id) {
            index.as_ref().clone()
        } else {
            load_hosts(resource, HostsLimits::default())
                .map(|loaded| loaded.index().clone())
                .map_err(|source| match source {
                    ResourceLoadError::Parse { source, .. } => PolicyBuildError::HostsParse {
                        resource: id.clone(),
                        source,
                    },
                    source => PolicyBuildError::HostsLoad {
                        resource: id.clone(),
                        source,
                    },
                })?
        };
        if host_indexes.insert(id.clone(), Arc::new(index)).is_some() {
            return Err(PolicyBuildError::DuplicateHostsResource(id.clone()));
        }
    }

    let mut rule_indexes = BTreeMap::new();
    for resource in rule_sets {
        let id = match resource {
            ResolvedRuleSet::Const { id, .. }
            | ResolvedRuleSet::File { id, .. }
            | ResolvedRuleSet::Remote { id, .. } => id,
        };
        let index = if let Some(index) = supplied_rule_indexes.get(id) {
            index.clone()
        } else {
            match resource {
                ResolvedRuleSet::Remote { .. } => {
                    return Err(PolicyBuildError::UnsupportedRuleSet {
                        resource: id.clone(),
                        kind: "remote",
                    });
                }
                ResolvedRuleSet::Const { format, .. } | ResolvedRuleSet::File { format, .. } => {
                    if *format == RuleSetFormat::Dat {
                        return Err(PolicyBuildError::UnsupportedRuleSet {
                            resource: id.clone(),
                            kind: "dat",
                        });
                    }
                    Arc::new(
                        load_rule_set(resource, RuleLimits::default())
                            .map_err(|source| match source {
                                RuleResourceLoadError::Parse { source, .. } => {
                                    PolicyBuildError::RuleSetParse {
                                        resource: id.clone(),
                                        source,
                                    }
                                }
                                RuleResourceLoadError::UnsupportedFormat { .. } => {
                                    PolicyBuildError::UnsupportedRuleSet {
                                        resource: id.clone(),
                                        kind: "dat",
                                    }
                                }
                                RuleResourceLoadError::UnsupportedSource { kind, .. } => {
                                    PolicyBuildError::UnsupportedRuleSet {
                                        resource: id.clone(),
                                        kind,
                                    }
                                }
                                source => PolicyBuildError::RuleSetLoad {
                                    resource: id.clone(),
                                    source,
                                },
                            })?
                            .index()
                            .clone(),
                    )
                }
            }
        };
        if rule_indexes.insert(id.clone(), index).is_some() {
            return Err(PolicyBuildError::DuplicateRuleSet(id.clone()));
        }
    }
    Ok((host_indexes, rule_indexes))
}

fn strategy_namespace(strategy: &ResolvedStrategy) -> Result<CacheStrategyId, PolicyError> {
    CacheStrategyId::from_validated_config_id(strategy.id.as_str())
        .map_err(|_| PolicyError::CacheStrategyIdInvalid(strategy.id.clone()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use ipnet::IpNet;

    use crate::config::model::EcsMode;
    use crate::config::resolve::{
        ConfigId, ResolvedCacheOverride, ResolvedClient, ResolvedEcs, ResolvedGlobalCache,
        ResolvedHostsResource, ResolvedListener, ResolvedOptimistic, ResolvedRuleSetRef,
        ResolvedStrategy, ResolvedStrategyRule, ResolvedTtlOverride, ValueSource,
    };
    use crate::ports::cache::ClientCacheDigest;
    use crate::resource::{CanonicalDomain, HostsIndex, RuleIndex};

    use super::{CacheDecision, MatchedRuleKind, PolicyError, PolicyIndex, PolicyRequest};

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

    fn strategy_with_rules(name: &str, rules: Vec<ResolvedStrategyRule>) -> ResolvedStrategy {
        ResolvedStrategy {
            id: ConfigId::new(name).unwrap(),
            rules,
            default_upstream: ConfigId::new("upstream").unwrap(),
            cache: None,
            ttl_override: ttl(),
            edns_client_subnet: ecs(),
        }
    }

    fn strategy_rule(
        hosts: Option<&str>,
        rule_set: Option<&str>,
        upstream: Option<&str>,
    ) -> ResolvedStrategyRule {
        ResolvedStrategyRule {
            hosts: hosts.map(|value| ConfigId::new(value).unwrap()),
            rule_set: rule_set.map(|value| ResolvedRuleSetRef {
                resource: ConfigId::new(value).unwrap(),
                selector: None,
            }),
            upstream: upstream.map(|value| ConfigId::new(value).unwrap()),
            edns_client_subnet: ecs(),
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
                qname: None,
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
    fn client_without_ttl_override_inherits_selected_strategy() {
        let mut inner = strategy("inner", None);
        inner.ttl_override = ResolvedTtlOverride {
            enabled: true,
            min: None,
            max: Some(Duration::from_secs(30)),
            source: ValueSource::Strategy,
        };
        let mut matched_client = client("client1", Some("inner"), None);
        matched_client.ttl_override = ResolvedTtlOverride {
            enabled: true,
            min: None,
            max: Some(Duration::from_secs(120)),
            source: ValueSource::Global,
        };
        let index = PolicyIndex::build(
            [listener()],
            [strategy("default", None), inner],
            [matched_client],
            global(false),
        )
        .unwrap();
        let listener_id = ConfigId::new("lan").unwrap();

        let plan = index
            .evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: None,
                client_id: Some("alice"),
                client_addr: Some(IpAddr::from([192, 0, 2, 10])),
                client_digest: None,
                qname: None,
            })
            .unwrap();

        assert_eq!(plan.ttl_override.source, ValueSource::Strategy);
        assert_eq!(plan.ttl_override.max, Some(Duration::from_secs(30)));
    }

    #[test]
    fn client_without_ecs_override_inherits_selected_strategy() {
        let mut inner = strategy("inner", None);
        inner.edns_client_subnet = ResolvedEcs {
            mode: EcsMode::Custom,
            custom_ip: Some("203.0.113.0/24".parse().unwrap()),
            source: ValueSource::Strategy,
        };
        let mut matched_client = client("client1", Some("inner"), None);
        matched_client.edns_client_subnet = ResolvedEcs {
            mode: EcsMode::Disabled,
            custom_ip: None,
            source: ValueSource::Global,
        };
        let index = PolicyIndex::build(
            [listener()],
            [strategy("default", None), inner],
            [matched_client],
            global(false),
        )
        .unwrap();
        let listener_id = ConfigId::new("lan").unwrap();

        let plan = index
            .evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: None,
                client_id: Some("alice"),
                client_addr: Some(IpAddr::from([192, 0, 2, 10])),
                client_digest: None,
                qname: None,
            })
            .unwrap();

        assert_eq!(plan.edns_client_subnet.source, ValueSource::Strategy);
        assert_eq!(
            plan.edns_client_subnet.custom_ip,
            Some("203.0.113.0/24".parse().unwrap())
        );
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
                qname: None,
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
                qname: None,
            })
            .unwrap();
        assert_eq!(plan.cache, CacheDecision::Disabled);
    }

    #[test]
    fn listener_hosts_precede_ordered_strategy_resource_rules() {
        let listener_hosts = ConfigId::new("listener-hosts").unwrap();
        let strategy_hosts = ConfigId::new("strategy-hosts").unwrap();
        let rule_set = ConfigId::new("rules").unwrap();
        let mut listener = listener();
        if let ResolvedListener::Udp { hosts, .. } = &mut listener {
            *hosts = Some(listener_hosts.clone());
        }

        let strategy = strategy_with_rules(
            "default",
            vec![
                strategy_rule(Some(strategy_hosts.as_str()), None, None),
                strategy_rule(None, Some(rule_set.as_str()), Some("blocked")),
            ],
        );
        let mut hosts = std::collections::BTreeMap::new();
        hosts.insert(
            listener_hosts.clone(),
            Arc::new(HostsIndex::parse_hosts("192.0.2.1 listener.example\n").unwrap()),
        );
        hosts.insert(
            strategy_hosts.clone(),
            Arc::new(HostsIndex::parse_hosts("192.0.2.2 shared.example\n").unwrap()),
        );
        let mut rule_sets = std::collections::BTreeMap::new();
        rule_sets.insert(
            rule_set.clone(),
            Arc::new(
                RuleIndex::parse_json(
                    r#"{"domain":["listener.example","shared.example","rule.example"]}"#,
                )
                .unwrap(),
            ),
        );
        let index = PolicyIndex::build_with_resources(
            [listener],
            [strategy],
            [],
            BTreeMap::new(),
            global(false),
            hosts,
            rule_sets,
        )
        .unwrap();
        let listener_id = ConfigId::new("lan").unwrap();

        let listener_name = CanonicalDomain::parse("listener.example").unwrap();
        let listener_plan = index
            .evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: None,
                client_id: None,
                client_addr: None,
                client_digest: None,
                qname: Some(&listener_name),
            })
            .unwrap();
        assert_eq!(listener_plan.hosts, Some(listener_hosts.clone()));
        assert_eq!(listener_plan.upstream.as_str(), "upstream");
        assert!(matches!(
            listener_plan.matched_rule.as_ref().map(|matched| &matched.kind),
            Some(MatchedRuleKind::ListenerHosts { resource }) if resource == &listener_hosts
        ));

        let shared_name = CanonicalDomain::parse("shared.example").unwrap();
        let shared_plan = index
            .evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: None,
                client_id: None,
                client_addr: None,
                client_digest: None,
                qname: Some(&shared_name),
            })
            .unwrap();
        assert_eq!(shared_plan.hosts, Some(strategy_hosts));
        assert_eq!(shared_plan.matched_rule.as_ref().unwrap().ordinal, 0);

        let rule_name = CanonicalDomain::parse("rule.example").unwrap();
        let rule_plan = index
            .evaluate(PolicyRequest {
                listener_id: &listener_id,
                doh_path: None,
                client_id: None,
                client_addr: None,
                client_digest: None,
                qname: Some(&rule_name),
            })
            .unwrap();
        assert_eq!(rule_plan.upstream.as_str(), "blocked");
        assert!(matches!(
            rule_plan.matched_rule.as_ref().map(|matched| &matched.kind),
            Some(MatchedRuleKind::RuleSet { resource, .. }) if resource == &rule_set
        ));
    }

    #[test]
    fn missing_resource_is_reported_at_evaluation_boundary() {
        let missing = ConfigId::new("missing-rules").unwrap();
        let strategy = strategy_with_rules(
            "default",
            vec![ResolvedStrategyRule {
                rule_set: Some(ResolvedRuleSetRef {
                    resource: missing.clone(),
                    selector: None,
                }),
                hosts: None,
                upstream: Some(ConfigId::new("blocked").unwrap()),
                edns_client_subnet: ecs(),
            }],
        );
        let index = PolicyIndex::build([listener()], [strategy], [], global(false)).unwrap();
        let listener_id = ConfigId::new("lan").unwrap();
        let qname = CanonicalDomain::parse("missing.example").unwrap();
        let result = index.evaluate(PolicyRequest {
            listener_id: &listener_id,
            doh_path: None,
            client_id: None,
            client_addr: None,
            client_digest: None,
            qname: Some(&qname),
        });

        assert_eq!(result, Err(PolicyError::RuleSetNotFound(missing)));
    }

    #[test]
    fn policy_loads_const_and_file_resources_at_build_boundary() {
        let hosts_id = ConfigId::new("file-hosts").unwrap();
        let rules_id = ConfigId::new("file-rules").unwrap();
        let path = std::env::temp_dir().join(format!(
            "fluxdns-policy-resource-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "192.0.2.9 file.example\n").unwrap();
        let result = super::compile_resources(
            &[ResolvedHostsResource::File {
                id: hosts_id.clone(),
                format: crate::config::model::HostsFormat::Hosts,
                path: path.clone(),
                auto_update: false,
                update_interval: None,
            }],
            &[crate::config::resolve::ResolvedRuleSet::Const {
                id: rules_id.clone(),
                format: crate::config::model::RuleSetFormat::Json,
                rule: r#"{"domain":"rule.example"}"#.to_owned(),
            }],
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(result.0.get(&hosts_id).unwrap().record_count(), 1);
        assert_eq!(result.1.get(&rules_id).unwrap().rule_count(), 1);
        let _ = std::fs::remove_file(path);
    }
}
