//! Immutable strategy lookup index。

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::resolve::{ConfigId, ResolvedConfig, ResolvedStrategy};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StrategyBuildError {
    DuplicateStrategy(ConfigId),
}

#[derive(Clone, Debug, Default)]
pub struct StrategyIndex {
    strategies: BTreeMap<ConfigId, Arc<ResolvedStrategy>>,
}

impl StrategyIndex {
    pub fn build(
        strategies: impl IntoIterator<Item = ResolvedStrategy>,
    ) -> Result<Self, StrategyBuildError> {
        let mut index = Self::default();
        for strategy in strategies {
            let id = strategy.id.clone();
            if index
                .strategies
                .insert(id.clone(), Arc::new(strategy))
                .is_some()
            {
                return Err(StrategyBuildError::DuplicateStrategy(id));
            }
        }
        Ok(index)
    }

    pub fn from_config(config: &ResolvedConfig) -> Result<Self, StrategyBuildError> {
        Self::build(config.strategies.clone())
    }

    pub fn len(&self) -> usize {
        self.strategies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strategies.is_empty()
    }

    pub fn get(&self, id: &ConfigId) -> Option<Arc<ResolvedStrategy>> {
        self.strategies.get(id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use crate::config::model::EcsMode;
    use crate::config::resolve::{
        ConfigId, ResolvedEcs, ResolvedStrategy, ResolvedTtlOverride, ValueSource,
    };

    use super::{StrategyBuildError, StrategyIndex};

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

    #[test]
    fn looks_up_immutable_strategy_by_id() {
        let index = StrategyIndex::build([strategy("default"), strategy("inner")]).unwrap();
        let found = index.get(&ConfigId::new("inner").unwrap()).unwrap();
        assert_eq!(found.id.as_str(), "inner");
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn rejects_duplicate_strategy_ids() {
        let error = StrategyIndex::build([strategy("same"), strategy("same")]).unwrap_err();
        assert_eq!(
            error,
            StrategyBuildError::DuplicateStrategy(ConfigId::new("same").unwrap())
        );
    }
}
