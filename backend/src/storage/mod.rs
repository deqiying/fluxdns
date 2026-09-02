//! Storage 的纯业务域逻辑。
//!
//! 本模块只负责内存统计、epoch checkpoint 和批次幂等状态；SQLite adapter、migration
//! 与 writer 装配由后续阶段接入 `StorageBackend`。

mod ledger;
mod resolve_log;
mod sqlite;
mod statistics;
mod writer;

pub use ledger::{BatchDecision, BatchLedger, BatchLedgerError, BatchReceipt, PendingStatsBatch};
pub use resolve_log::{
    ResolveDetailRecord, ResolveDetailWriter, ResolveLogBuildError, ResolveLogFlushSummary,
    ResolveLogShutdownSummary, ResolveLogWriter,
};
pub use sqlite::{SqliteStorageBackend, SqliteStorageBackendBuildError};
pub use statistics::{
    DimensionCount, PersistenceGapState, StatsAccumulator, StatsAccumulatorError, StatsSnapshot,
    day_utc,
};
pub use writer::{InMemoryStorageBackend, STORAGE_SCHEMA_VERSION};
