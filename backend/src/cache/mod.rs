//! CacheStore 的内存实现。

mod admission;
mod key;
mod memory;
mod service;

pub use admission::{
    CacheAdmissionError, CacheAdmissionOutcome, CacheAdmissionPolicy, CacheAdmissionRejection,
    admit_response, canonical_checksum,
};
pub use key::{
    CACHE_KEY_FORMAT_VERSION, CacheFingerprint, CacheKeyDimensions, CacheKeyError, build_cache_key,
};
pub use memory::MemoryCacheStore;
pub use service::{
    CacheFacade, CacheFacadeError, CacheFacadeOptions, CacheLookup, CacheRefreshPermit,
    CacheWriteRequest, CacheWriteResult,
};
