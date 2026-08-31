//! Listener/DoH route 到基础 strategy 的不可变索引。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

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
    InvalidPath,
    InvalidPlaceholder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutePattern {
    template: String,
    placeholder: Option<(usize, usize)>,
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
        routes: Vec<(RoutePattern, ConfigId)>,
    },
}

impl RoutePattern {
    pub fn new(template: impl Into<String>) -> Result<Self, RouteBuildError> {
        let template = template.into();
        if template.is_empty()
            || !template.starts_with('/')
            || template.contains('?')
            || template.contains('#')
            || template
                .as_bytes()
                .iter()
                .any(|byte| *byte < 0x20 || *byte == 0x7f)
        {
            return Err(RouteBuildError::InvalidPath);
        }
        let marker = "{client_id}";
        let first = template.find(marker);
        if first.is_some_and(|index| template[index + marker.len()..].contains(marker)) {
            return Err(RouteBuildError::InvalidPlaceholder);
        }
        let placeholder = first.map(|start| (start, start + marker.len()));
        if let Some((start, end)) = placeholder {
            let segment_start = start == 0 || template.as_bytes()[start - 1] == b'/';
            let segment_end = end == template.len() || template.as_bytes()[end] == b'/';
            if !segment_start || !segment_end {
                return Err(RouteBuildError::InvalidPlaceholder);
            }
        }
        Ok(Self {
            template,
            placeholder,
        })
    }

    pub fn template(&self) -> &str {
        &self.template
    }

    pub fn matches(&self, path: &str) -> Option<RouteMatch> {
        let client_id = match self.placeholder {
            None if path == self.template => None,
            Some((start, end)) => {
                if !path.starts_with(&self.template[..start])
                    || !path.ends_with(&self.template[end..])
                {
                    return None;
                }
                let value_end = path.len().checked_sub(self.template.len() - end)?;
                let value = &path[start..value_end];
                if value.is_empty() || value.contains('/') || value.contains('?') {
                    return None;
                }
                Some(ClientId::new(value.to_owned()))
            }
            _ => return None,
        };
        Some(RouteMatch {
            route_id: RouteId::from(self.template.clone()),
            client_id,
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
                    let mut compiled = Vec::with_capacity(routes.len());
                    for route in routes {
                        ensure_strategy(&index.strategies, &id, &route.strategy)?;
                        let pattern = RoutePattern::new(route.path)?;
                        compiled.push((pattern, route.strategy));
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

    pub fn select_doh(&self, listener_id: &ConfigId, path: &str) -> Option<RouteSelection> {
        let ListenerRoutes::Doh { routes } = self.listeners.get(listener_id)? else {
            return None;
        };
        routes.iter().find_map(|(pattern, strategy)| {
            let route = pattern.matches(path)?;
            let strategy_value = self.strategies.get(strategy)?;
            Some(RouteSelection {
                listener_id: listener_id.clone(),
                route: Some(route),
                strategy: strategy_value,
                listener_hosts: None,
            })
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
                .select_doh(&ConfigId::new("doh").unwrap(), "/clients/alice")
                .is_some()
        );
        assert!(
            index
                .select_doh(&ConfigId::new("doh").unwrap(), "/unknown")
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
}
