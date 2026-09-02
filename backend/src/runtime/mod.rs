//! 运行时状态、候选 prepare 和 listener 绑定生命周期。

mod bind;
mod coordinator;
mod prepared;
mod snapshot;
mod supervisor;
mod system_clock;
mod system_socket;

pub use bind::{BindError, BoundCandidate, BoundEndpointHandle, BoundListenerSet, bind_prepared};
pub use coordinator::{
    ActivationError, ActiveRuntime, AdmissionError, RequestGuard, ResourceRefreshCoordinatorError,
    RuntimeCoordinator, RuntimeLease, RuntimeReloadError, RuntimeReuseError,
};
pub use prepared::{
    PreflightReport, PrepareError, PreparedRuntime, RefreshedResourceSnapshot, ResourceRefreshError,
};
pub use snapshot::{RuntimeSnapshot, RuntimeSnapshotSummary};
pub use supervisor::{
    FaultLevel, RestartPolicy, ShutdownReport, Supervisor, SupervisorError, TaskCompletion,
    TaskError, TaskErrorKind, TaskExit, TaskFuture, TaskId, TaskIdError, TaskSpec,
};
pub use system_clock::SystemClock;
pub use system_socket::SystemSocketFactory;
