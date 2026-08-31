//! 纯内存策略索引与请求决策。

mod client;
mod strategy;

pub use client::{ClientIndex, ClientMatch, ClientMatchKind, ClientRule, ClientRuleBuildError};
pub use strategy::{StrategyBuildError, StrategyIndex};
