//! Storage 的纯业务域逻辑。
//!
//! 本模块负责内存统计、epoch checkpoint、批次幂等状态以及 SQLite/stats/service writer 生命周期边界。

mod ledger;
mod resolve_log;
mod service;
mod sqlite;
mod statistics;
mod stats;
mod writer;

#[cfg(test)]
mod backend_contract_tests;

pub use ledger::{BatchDecision, BatchLedger, BatchLedgerError, BatchReceipt, PendingStatsBatch};
pub use resolve_log::{
    ResolveDetailRecord, ResolveDetailWriter, ResolveLogBuildError, ResolveLogFlushSummary,
    ResolveLogShutdownSummary, ResolveLogWriter,
};
pub use service::{
    DEFAULT_RESOLVE_LOG_BATCH_SIZE, DEFAULT_RESOLVE_LOG_QUEUE_CAPACITY,
    DEFAULT_STORAGE_FLUSH_INTERVAL, DEFAULT_STORAGE_OPERATION_TIMEOUT, StorageRuntime,
    StorageRuntimeBuildError, StorageService, StorageServiceError, StorageServiceFlushSummary,
};
pub use sqlite::{
    SqliteResolveDetailFlushSummary, SqliteResolveDetailLimits, SqliteResolveDetailRunSummary,
    SqliteResolveDetailWorker, SqliteResolveDetailWriter, SqliteResolveDetailWriterBuildError,
    SqliteStorageBackend, SqliteStorageBackendBuildError,
};
pub use statistics::{
    DimensionCount, PersistenceGapState, StatsAccumulator, StatsAccumulatorError, StatsSnapshot,
    day_utc,
};
pub use stats::{
    MAX_PENDING_STATS_BATCHES, MAX_PENDING_STATS_EVENTS, StatsPendingLimit, StatsPersistenceError,
    StatsPersistenceFlushSummary, StatsPersistenceWorker,
};
pub use writer::{InMemoryStorageBackend, STORAGE_SCHEMA_VERSION};
