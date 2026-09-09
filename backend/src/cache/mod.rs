//! CacheStore 的内存实现。

mod admission;
mod codec;
mod key;
mod memory;
mod moka;
mod service;
mod snapshot;
mod snapshot_owner;

pub use admission::{
    CacheAdmissionError, CacheAdmissionOutcome, CacheAdmissionPolicy, CacheAdmissionRejection,
    admit_response, canonical_checksum,
};
pub use key::{
    CACHE_KEY_FORMAT_VERSION, CacheFingerprint, CacheKeyDimensions, CacheKeyError, CacheKeyMode,
    build_cache_key,
};
pub use memory::{MemoryCacheStore, MemoryCacheStoreBuildError};
pub use moka::{MokaCacheStore, MokaCacheStoreBuildError};
pub use service::{
    CacheCommitCandidate, CacheCommitOutcome, CacheFacade, CacheFacadeBuildError, CacheFacadeError,
    CacheFacadeOptions, CacheLookup, CacheRefreshPermit, CacheWriteRequest, CacheWriteResult,
    LateCacheFinalizer, LateCacheFinalizerBuildError, LateCacheFinalizerShutdownSummary,
    LateCacheFinalizerSubmitError,
};
pub use snapshot::{
    CacheSnapshotError, CacheSnapshotReader, CacheSnapshotWriteSummary, open_cache_snapshot,
    write_cache_snapshot,
};
pub(crate) use snapshot_owner::{
    CacheSnapshotCondition, CacheSnapshotFailure, CacheSnapshotOwner, CacheSnapshotOwnerBuildError,
    CacheSnapshotOwnerStatus, CacheSnapshotSettings, CacheSnapshotShutdownSummary,
    PreparedCacheSnapshotSwitch,
};
#[cfg(test)]
mod backend_contract_tests;
