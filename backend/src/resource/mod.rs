//! 可热替换的资源解析与不可变索引。

mod fetcher;
mod hosts;
mod loader;
mod orchestrator;
mod refresh;
mod remote;
mod rules;
mod scheduler;
mod snapshot;

pub use fetcher::{ReqwestResourceFetcher, ResourceFetcherBuildError};
pub use hosts::{
    CanonicalDomain, HostsIndex, HostsLimits, HostsLookup, HostsParseError, HostsRecord,
};
pub use loader::{
    FileFingerprint, LoadedHostsResource, LoadedRuleSetResource, ResourceLoadError, ResourceSource,
    RuleResourceLoadError, load_hosts, load_rule_set,
};
pub use orchestrator::{
    FileHostsRefreshWorker, FileRuleSetRefreshWorker, LocalResourceRefreshWorkerError,
    ResourceRefreshRuntime, ResourceRefreshRuntimeBeginError, ResourceRefreshRuntimePermit,
    ResourceRefreshWorker, ResourceRefreshWorkerError,
};
pub use refresh::{
    RefreshBackoff, RefreshBeginError, RefreshFailure, RefreshPermit, RefreshPublishError,
    ResourceRefreshCoordinator, ResourceRefreshPhase, ResourceRefreshStatus,
};
pub use remote::{
    LoadedRemoteRuleSet, RemoteResourceError, RemoteResourceManifest, RemoteResourceOptions,
    fetch_remote_rule_set, restore_remote_rule_set,
};
pub use rules::{RuleIndex, RuleLimits, RuleMatch, RuleParseError};
pub use scheduler::{
    ResourceSchedule, ResourceScheduleDecision, ResourceSchedulePolicy, ResourceScheduleStopReason,
};
pub use snapshot::{
    ResourcePublishError, ResourceRegistrySnapshot, ResourceSnapshot, ResourceSourceKind,
    ResourceStaleStatus, ResourceSummary, ResourceVersion,
};
