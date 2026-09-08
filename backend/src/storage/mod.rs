//! Storage 的纯业务域逻辑。
//!
//! 本模块负责内存统计、epoch checkpoint、批次幂等状态以及 SQLite/stats/service writer 生命周期边界。

mod detail_query;
mod detail_shards;
mod ledger;
mod management_read;
mod resolve_log;
mod retention;
mod service;
mod sqlite;
mod statistics;
mod stats;
mod writer;

#[cfg(test)]
mod backend_contract_tests;

pub use detail_query::{
    DetailCommitCursor, DetailCommitNotification, DetailCommittedRecord, DetailPageDirection,
    DetailQuery, DetailQueryCacheOutcome, DetailQueryFilter, DetailQueryOutcome, DetailQueryPage,
    DetailQueryRcode, DetailQueryRecord, DetailQuerySort, DetailQuerySource, DetailQueryTransport,
    DetailRecordId, DetailSortOrder,
};
pub use detail_shards::{
    DEFAULT_MAX_ACTIVE_DETAIL_SHARDS, DETAIL_SHARD_LAYOUT_VERSION, DetailShardStore,
    DetailShardStoreBuildError, ShardedResolveDetailWorker, ShardedResolveDetailWriter,
    ShardedResolveDetailWriterBuildError,
};
pub use ledger::{BatchDecision, BatchLedger, BatchLedgerError, BatchReceipt, PendingStatsBatch};
pub use management_read::{SqliteManagementReadModel, SqliteManagementReadModelBuildError};
pub use resolve_log::ResolveDetailRecord;
pub(crate) use retention::RetentionScheduler;
pub use retention::{
    DEFAULT_RETENTION_DAYS, DEFAULT_RETENTION_GRACE_DAYS, DEFAULT_RETENTION_OPERATION_TIMEOUT,
    DEFAULT_RETENTION_POLL_INTERVAL, DEFAULT_RETENTION_REFERENCE_SIZE_BYTES,
    DEFAULT_RETENTION_RETRY_INTERVAL, MAX_RETENTION_DAYS, MAX_RETENTION_REFERENCE_SIZE_BYTES,
    RETENTION_SCHEDULE_LOCAL_SECOND, RetentionAvailableRange, RetentionCoordinator, RetentionError,
    RetentionManifestEntry, RetentionManifestState, RetentionPlan, RetentionPolicy,
    RetentionPolicyError, RetentionReclaimSummary, RetentionRunState, RetentionSchedulerSummary,
    RetentionState, RetentionStatus,
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
