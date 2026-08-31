//! 已解析 upstream 的 connector 实现。

mod executor;
mod group;
mod hosts;
mod outcome;
mod registry;

pub use executor::{ExecutorBuildError, ExecutorError, UpstreamGroupExecutor};
pub use group::{GroupSelector, GroupSelectorError, SelectionLease};
pub use hosts::{HostsExchange, HostsExchangeBuildError};
pub use outcome::{
    AttemptAssessment, AttemptClass, FallbackDecision, OutcomeError, UpstreamAttempt, aggregate,
    assess, deduplicate_connector_order,
};
pub use registry::{RegistryError, UpstreamRegistry};
