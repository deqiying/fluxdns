//! 可热替换的资源解析与不可变索引。

mod hosts;
mod loader;
mod rules;
mod snapshot;

pub use hosts::{
    CanonicalDomain, HostsIndex, HostsLimits, HostsLookup, HostsParseError, HostsRecord,
};
pub use loader::{
    FileFingerprint, LoadedHostsResource, ResourceLoadError, ResourceSource, load_hosts,
};
pub use rules::{RuleIndex, RuleLimits, RuleMatch, RuleParseError};
pub use snapshot::{
    ResourcePublishError, ResourceRegistrySnapshot, ResourceSnapshot, ResourceSourceKind,
    ResourceStaleStatus, ResourceSummary, ResourceVersion,
};
