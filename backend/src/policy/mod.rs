//! 纯内存策略索引与请求决策。

mod client;
mod plan;
mod route;
mod strategy;

pub use client::{ClientIndex, ClientMatch, ClientMatchKind, ClientRule, ClientRuleBuildError};
pub use plan::{
    CacheDecision, MatchedRule, MatchedRuleKind, PolicyBuildError, PolicyContext, PolicyError,
    PolicyIndex, PolicyRequest, ResolutionPlan, RouteDecision,
};
pub use route::{RouteBuildError, RouteIndex, RouteMatch, RoutePattern, RouteSelection};
pub use strategy::{StrategyBuildError, StrategyIndex};
