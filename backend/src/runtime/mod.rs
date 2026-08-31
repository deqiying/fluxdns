//! 运行时状态、候选 prepare 和 listener 绑定生命周期。

mod bind;
mod coordinator;
mod prepared;
mod snapshot;

pub use bind::{BindError, BoundCandidate, BoundListenerSet, bind_prepared};
pub use coordinator::{
    ActivationError, ActiveRuntime, AdmissionError, RequestGuard, RuntimeCoordinator, RuntimeLease,
};
pub use prepared::{PreflightReport, PrepareError, PreparedRuntime};
pub use snapshot::{RuntimeSnapshot, RuntimeSnapshotSummary};
