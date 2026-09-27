//! Listener/DoH route 到基础 strategy 的不可变索引。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::config::doh_route::{DohPathPattern, DohPathPatternError};
use crate::config::resolve::{ConfigId, ResolvedConfig, ResolvedListener, ResolvedStrategy};
use crate::dns::{ClientId, RouteId};

use super::{StrategyBuildError, StrategyIndex};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteBuildError {
    Strategy(StrategyBuildError),
    DuplicateListener(ConfigId),
    MissingStrategy {
        listener: ConfigId,
        strategy: ConfigId,
    },
    DuplicateRoute {
        listener: ConfigId,
        route: RouteId,
    },
    InvalidPath,
    InvalidPlaceholder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutePattern {
    path: DohPathPattern,
}

#[derive(Clone, Eq, PartialEq)]
pub struct RouteMatch {
    pub route_id: RouteId,
    pub client_id: Option<ClientId>,
}

impl fmt::Debug for RouteMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteMatch")
            .field("route_id", &self.route_id)
            .field("has_client_id", &self.client_id.is_some())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct RouteSelection {
    pub listener_id: ConfigId,
    pub route: Option<RouteMatch>,
    pub strategy: Arc<ResolvedStrategy>,
    pub listener_hosts: Option<ConfigId>,
}

#[derive(Clone, Debug, Default)]
pub struct RouteIndex {
    strategies: StrategyIndex,
    listeners: BTreeMap<ConfigId, ListenerRoutes>,
}

#[derive(Clone, Debug)]
enum ListenerRoutes {
    Stream {
        strategy: ConfigId,
        hosts: Option<ConfigId>,
    },
    Doh {
        routes: BTreeMap<RouteId, ConfigId>,
    },
}

impl RoutePattern {
    pub fn new(template: impl Into<String>) -> Result<Self, RouteBuildError> {
        let path = DohPathPattern::new(template).map_err(|error| match error {
            DohPathPatternError::InvalidPath => RouteBuildError::InvalidPath,
            DohPathPatternError::InvalidPlaceholder => RouteBuildError::InvalidPlaceholder,
        })?;
        Ok(Self { path })
    }

    pub fn template(&self) -> &str {
        self.path.template()
    }

    pub fn matches(&self, path: &str) -> Option<RouteMatch> {
        let matched = self.path.matches(path)?;
        Some(RouteMatch {
            route_id: RouteId::from(self.path.template().to_owned()),
            client_id: matched
                .client_id
                .map(|value| ClientId::new(value.to_owned())),
        })
    }
}

impl RouteIndex {
    pub fn build(
        listeners: impl IntoIterator<Item = ResolvedListener>,
        strategies: impl IntoIterator<Item = ResolvedStrategy>,
    ) -> Result<Self, RouteBuildError> {
        let strategies = StrategyIndex::build(strategies).map_err(RouteBuildError::Strategy)?;
        let mut index = Self {
            strategies,
            listeners: BTreeMap::new(),
        };
        for listener in listeners {
            let (id, routes) = match listener {
                ResolvedListener::Udp {
                    id,
                    strategy,
                    hosts,
                    ..
                }
                | ResolvedListener::Tcp {
                    id,
                    strategy,
                    hosts,
                    ..
                } => {
                    ensure_strategy(&index.strategies, &id, &strategy)?;
                    (id, ListenerRoutes::Stream { strategy, hosts })
                }
                ResolvedListener::Doh { id, routes, .. } => {
                    let mut compiled = BTreeMap::new();
                    for route in routes {
                        ensure_strategy(&index.strategies, &id, &route.strategy)?;
                        let pattern = RoutePattern::new(route.path)?;
                        let route_id = RouteId::from(pattern.template().to_owned());
                        if compiled.insert(route_id.clone(), route.strategy).is_some() {
                            return Err(RouteBuildError::DuplicateRoute {
                                listener: id,
                                route: route_id,
                            });
                        }
                    }
                    (id, ListenerRoutes::Doh { routes: compiled })
                }
            };
            if index.listeners.insert(id.clone(), routes).is_some() {
                return Err(RouteBuildError::DuplicateListener(id));
            }
        }
        Ok(index)
    }

    pub fn from_config(config: &ResolvedConfig) -> Result<Self, RouteBuildError> {
        Self::build(config.listeners.clone(), config.strategies.clone())
    }

    pub fn listener_count(&self) -> usize {
        self.listeners.len()
    }

    pub fn strategy(&self, id: &ConfigId) -> Option<Arc<ResolvedStrategy>> {
        self.strategies.get(id)
    }

    pub fn select_stream(&self, listener_id: &ConfigId) -> Result<RouteSelection, RouteBuildError> {
        let Some(ListenerRoutes::Stream { strategy, hosts }) = self.listeners.get(listener_id)
        else {
            return Err(RouteBuildError::MissingStrategy {
                listener: listener_id.clone(),
                strategy: listener_id.clone(),
            });
        };
        let strategy_value =
            self.strategies
                .get(strategy)
                .ok_or_else(|| RouteBuildError::MissingStrategy {
                    listener: listener_id.clone(),
                    strategy: strategy.clone(),
                })?;
        Ok(RouteSelection {
            listener_id: listener_id.clone(),
            route: None,
            strategy: strategy_value,
            listener_hosts: hosts.clone(),
        })
    }

    pub fn select_doh(&self, listener_id: &ConfigId, route_id: &RouteId) -> Option<RouteSelection> {
        let ListenerRoutes::Doh { routes } = self.listeners.get(listener_id)? else {
            return None;
        };
        let strategy = routes.get(route_id)?;
        let strategy_value = self.strategies.get(strategy)?;
        Some(RouteSelection {
            listener_id: listener_id.clone(),
            route: Some(RouteMatch {
                route_id: route_id.clone(),
                client_id: None,
            }),
            strategy: strategy_value,
            listener_hosts: None,
        })
    }
}

fn ensure_strategy(
    strategies: &StrategyIndex,
    listener: &ConfigId,
    strategy: &ConfigId,
) -> Result<(), RouteBuildError> {
    if strategies.get(strategy).is_none() {
        return Err(RouteBuildError::MissingStrategy {
            listener: listener.clone(),
            strategy: strategy.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use crate::config::model::EcsMode;
    use crate::config::resolve::{
        ConfigId, ResolvedEcs, ResolvedListener, ResolvedStrategy, ResolvedTtlOverride, ValueSource,
    };
    use crate::dns::RouteId;

    use super::{RouteBuildError, RouteIndex, RoutePattern};

    fn strategy(name: &str) -> ResolvedStrategy {
        ResolvedStrategy {
            id: ConfigId::new(name).unwrap(),
            rules: Vec::new(),
            default_upstream: ConfigId::new("upstream").unwrap(),
            cache: None,
            ttl_override: ResolvedTtlOverride {
                enabled: false,
                min: None,
                max: None,
                source: ValueSource::Default,
            },
            edns_client_subnet: ResolvedEcs {
                mode: EcsMode::Disabled,
                custom_ip: None,
                source: ValueSource::Default,
            },
        }
    }

    fn stream_listener(strategy: &str) -> ResolvedListener {
        ResolvedListener::Udp {
            id: ConfigId::new("lan").unwrap(),
            addresses: vec![IpAddr::from([127, 0, 0, 1])],
            port: 8353,
            strategy: ConfigId::new(strategy).unwrap(),
            hosts: None,
        }
    }

    #[test]
    fn selects_stream_strategy_and_doh_route() {
        let doh = ResolvedListener::Doh {
            id: ConfigId::new("doh").unwrap(),
            routes: vec![
                crate::config::resolve::ResolvedDohRoute {
                    path: "/dns-query".to_owned(),
                    strategy: ConfigId::new("default").unwrap(),
                    edns_client_subnet: None,
                },
                crate::config::resolve::ResolvedDohRoute {
                    path: "/clients/{client_id}".to_owned(),
                    strategy: ConfigId::new("inner").unwrap(),
                    edns_client_subnet: None,
                },
            ],
            endpoints: Vec::new(),
        };
        let index = RouteIndex::build(
            [stream_listener("default"), doh],
            [strategy("default"), strategy("inner")],
        )
        .unwrap();
        let stream = index.select_stream(&ConfigId::new("lan").unwrap()).unwrap();
        assert_eq!(stream.strategy.id.as_str(), "default");
        assert!(
            index
                .select_doh(
                    &ConfigId::new("doh").unwrap(),
                    &RouteId::from("/clients/{client_id}"),
                )
                .is_some()
        );
        assert!(
            index
                .select_doh(&ConfigId::new("doh").unwrap(), &RouteId::from("/unknown"),)
                .is_none()
        );
    }

    #[test]
    fn rejects_missing_strategy_and_invalid_placeholder() {
        let missing =
            RouteIndex::build([stream_listener("missing")], [strategy("default")]).unwrap_err();
        assert!(matches!(missing, RouteBuildError::MissingStrategy { .. }));
        assert!(matches!(
            RoutePattern::new("/clients/{client_id}/bad/{client_id}"),
            Err(RouteBuildError::InvalidPlaceholder)
        ));
    }

    #[test]
    fn terminal_client_id_pattern_includes_bare_path() {
        let pattern = RoutePattern::new("/clients/{client_id}").unwrap();

        let bare = pattern.matches("/clients").unwrap();
        assert_eq!(bare.route_id.as_ref(), "/clients/{client_id}");
        assert!(bare.client_id.is_none());
    }
}
