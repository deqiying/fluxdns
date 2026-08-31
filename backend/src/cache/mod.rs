//! CacheStore 的内存实现。

mod admission;
mod memory;

pub use admission::{
    CacheAdmissionError, CacheAdmissionOutcome, CacheAdmissionPolicy, CacheAdmissionRejection,
    admit_response, canonical_checksum,
};
pub use memory::MemoryCacheStore;
