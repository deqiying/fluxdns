//! Storage 的纯业务域逻辑。
//!
//! 本模块负责内存统计、epoch checkpoint、批次幂等状态以及 SQLite/service writer 生命周期边界。

mod ledger;
mod resolve_log;
mod service;
mod sqlite;
mod statistics;
mod writer;

pub use ledger::{BatchDecision, BatchLedger, BatchLedgerError, BatchReceipt, PendingStatsBatch};
pub use resolve_log::{
    ResolveDetailRecord, ResolveDetailWriter, ResolveLogBuildError, ResolveLogFlushSummary,
    ResolveLogShutdownSummary, ResolveLogWriter,
};
pub use service::{StorageService, StorageServiceError, StorageServiceFlushSummary};
pub use sqlite::{
    SqliteResolveDetailFlushSummary, SqliteResolveDetailLimits, SqliteResolveDetailRunSummary,
    SqliteResolveDetailWorker, SqliteResolveDetailWriter, SqliteResolveDetailWriterBuildError,
    SqliteStorageBackend, SqliteStorageBackendBuildError,
};
pub use statistics::{
    DimensionCount, PersistenceGapState, StatsAccumulator, StatsAccumulatorError, StatsSnapshot,
    day_utc,
};
pub use writer::{InMemoryStorageBackend, STORAGE_SCHEMA_VERSION};
