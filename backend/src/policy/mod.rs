//! 纯内存策略索引与请求决策。

mod client;

pub use client::{ClientIndex, ClientMatch, ClientMatchKind, ClientRule, ClientRuleBuildError};
