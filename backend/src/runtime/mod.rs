//! 运行时状态、候选 prepare 和 listener 绑定生命周期。

mod prepared;
mod snapshot;

pub use prepared::{PreflightReport, PrepareError, PreparedRuntime};
pub use snapshot::{RuntimeSnapshot, RuntimeSnapshotSummary};
