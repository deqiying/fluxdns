//! 已解析 upstream 的 connector 实现。

mod group;
mod hosts;
mod registry;

pub use group::{GroupSelector, GroupSelectorError, SelectionLease};
pub use hosts::{HostsExchange, HostsExchangeBuildError};
pub use registry::{RegistryError, UpstreamRegistry};
