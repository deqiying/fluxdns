//! Storage writer 的统一 flush/shutdown 生命周期边界。

use std::sync::Arc;
use std::time::Duration;

use crate::config::model::DatabaseType;
use crate::config::resolve::ResolvedConfig;
use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::PortError;
use crate::ports::storage::{StatsRecorder, StorageBackend, StorageFlushSummary};

use super::{
    DEFAULT_MAX_ACTIVE_DETAIL_SHARDS, DetailShardStore, DetailShardStoreBuildError,
    RetentionCoordinator, RetentionPolicy, RetentionScheduler, RetentionSchedulerSummary,
    STORAGE_SCHEMA_VERSION, ShardedResolveDetailWorker, ShardedResolveDetailWriter,
    ShardedResolveDetailWriterBuildError, SqliteResolveDetailFlushSummary,
    SqliteResolveDetailRunSummary, SqliteStorageBackend, SqliteStorageBackendBuildError,
    StatsPersistenceError, StatsPersistenceFlushSummary, StatsPersistenceWorker,
};

pub const DEFAULT_STORAGE_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
pub const DEFAULT_STORAGE_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_RESOLVE_LOG_QUEUE_CAPACITY: usize = 1_024;
pub const DEFAULT_RESOLVE_LOG_BATCH_SIZE: usize = 128;

/// Storage stats、backend 与 resolve detail worker 的一次生命周期汇总。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageServiceFlushSummary {
    pub stats: StatsPersistenceFlushSummary,
    pub storage: StorageFlushSummary,
    pub detail: SqliteResolveDetailFlushSummary,
}

/// 统一 writer 生命周期中的失败；shutdown 会尽力执行三个子边界后再返回。
#[derive(Debug, thiserror::Error)]
pub enum StorageServiceError {
    #[error("stats persistence operation failed: {0}")]
    Stats(#[source] StatsPersistenceError),
    #[error("storage backend operation failed: {0}")]
    Backend(#[source] PortError),
    #[error("resolve detail worker operation failed: {0}")]
    Detail(#[source] PortError),
    #[error("storage backend and resolve detail worker operations failed")]
    Both {
        detail: PortError,
        backend: PortError,
    },
    #[error("stats persistence and resolve detail worker operations failed")]
    StatsAndDetail {
        stats: StatsPersistenceError,
        detail: PortError,
    },
    #[error("stats persistence and storage backend operations failed")]
    StatsAndBackend {
        stats: StatsPersistenceError,
        backend: PortError,
    },
    #[error("stats persistence, storage backend and resolve detail worker operations failed")]
    All {
        stats: StatsPersistenceError,
        detail: PortError,
        backend: PortError,
    },
}

impl StorageServiceError {
    /// 判断任一存储子阶段是否以稳定的 timeout 分类失败。
    pub fn is_timeout(&self) -> bool {
        fn port_timeout(error: &PortError) -> bool {
            matches!(error.class(), crate::ports::PortErrorClass::Timeout)
        }

        fn stats_timeout(error: &StatsPersistenceError) -> bool {
            matches!(error, StatsPersistenceError::Backend(source) if port_timeout(source))
        }

        match self {
            Self::Stats(error) => stats_timeout(error),
            Self::Backend(error) | Self::Detail(error) => port_timeout(error),
            Self::Both { detail, backend } => port_timeout(detail) || port_timeout(backend),
            Self::StatsAndDetail { stats, detail } => stats_timeout(stats) || port_timeout(detail),
            Self::StatsAndBackend { stats, backend } => {
                stats_timeout(stats) || port_timeout(backend)
            }
            Self::All {
                stats,
                detail,
                backend,
            } => stats_timeout(stats) || port_timeout(detail) || port_timeout(backend),
        }
    }

    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::Stats(StatsPersistenceError::PendingLimitExceeded(_))
                | Self::StatsAndDetail {
                    stats: StatsPersistenceError::PendingLimitExceeded(_),
                    ..
                }
                | Self::StatsAndBackend {
                    stats: StatsPersistenceError::PendingLimitExceeded(_),
                    ..
                }
                | Self::All {
                    stats: StatsPersistenceError::PendingLimitExceeded(_),
                    ..
                }
        )
    }
}

/// 由已解析配置创建的业务存储运行时；持有数据面 sink 和 writer 生命周期。
pub struct StorageRuntime {
    service: StorageService,
    #[cfg(test)]
    backend_for_test: Arc<SqliteStorageBackend>,
    detail_store: Arc<DetailShardStore>,
    retention: Arc<RetentionCoordinator>,
    retention_cancellation: Option<Cancellation>,
    retention_task: Option<tokio::task::JoinHandle<RetentionSchedulerSummary>>,
    detail_writer: Option<ShardedResolveDetailWriter>,
    resolution_metrics: Arc<crate::resolution::ResolutionPipelineMetrics>,
    detail_cancellation: Option<Cancellation>,
    detail_task: Option<
        tokio::task::JoinHandle<
            Result<(ShardedResolveDetailWorker, SqliteResolveDetailRunSummary), PortError>,
        >,
    >,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageRuntimeBuildError {
    #[error("unsupported storage database type")]
    DatabaseType,
    #[error("sqlite storage could not be opened: {0}")]
    Connect(#[source] SqliteStorageBackendBuildError),
    #[error("sqlite storage migration failed: {0}")]
    Migration(#[source] PortError),
    #[error("sqlite storage startup write probe failed: {0}")]
    WriteProbe(#[source] PortError),
    #[error("resolve detail shard store is invalid: {0}")]
    DetailStore(#[source] DetailShardStoreBuildError),
    #[error("retention state could not be restored: {0}")]
    RetentionBootstrap(#[source] PortError),
    #[error("resolve detail channel could not be created: {0}")]
    DetailChannel(#[source] ShardedResolveDetailWriterBuildError),
    #[error("retention configuration is invalid: {0}")]
    RetentionPolicy(#[source] super::RetentionPolicyError),
}

/// 业务 Storage 的统一 flush/shutdown facade。
///
/// stats 与 detail worker 都必须在 backend 关闭前完成提交；shutdown 会先完成这两个
/// writer 的边界，再关闭 backend，避免 worker 使用已关闭的 pool。
pub struct StorageService {
    backend: Arc<dyn StorageBackend>,
    stats_worker: Option<Arc<StatsPersistenceWorker>>,
}

impl StorageService {
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self {
            backend,
            stats_worker: None,
        }
    }

    pub fn with_stats_worker(mut self, worker: Arc<StatsPersistenceWorker>) -> Self {
        self.stats_worker = Some(worker);
        self
    }

    pub fn has_stats_worker(&self) -> bool {
        self.stats_worker.is_some()
    }

    /// 返回由 resolution dispatcher 共享的同步 stats recorder。
    pub fn stats_recorder(&self) -> Option<Arc<dyn StatsRecorder>> {
        self.stats_worker
            .as_ref()
            .map(|worker| Arc::clone(worker) as Arc<dyn StatsRecorder>)
    }

    pub fn stats_worker(&self) -> Option<Arc<StatsPersistenceWorker>> {
        self.stats_worker.as_ref().map(Arc::clone)
    }

    /// 提交 stats 后 checkpoint 统计 backend；日分片 worker 由 StorageRuntime 单独持有。
    pub async fn flush(
        &mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
        let stats = match self.stats_worker.as_ref() {
            Some(worker) => worker
                .flush(deadline)
                .await
                .map_err(StorageServiceError::Stats)?,
            None => StatsPersistenceFlushSummary::default(),
        };
        let storage = self
            .backend
            .flush(deadline)
            .await
            .map_err(StorageServiceError::Backend)?;
        Ok(StorageServiceFlushSummary {
            stats,
            storage,
            detail: SqliteResolveDetailFlushSummary::default(),
        })
    }

    /// 在同一 deadline 内提交 stats 并关闭统计 backend。
    pub async fn shutdown(
        &mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
        let stats = match self.stats_worker.take() {
            Some(worker) => worker.flush(deadline).await,
            None => Ok(StatsPersistenceFlushSummary::default()),
        };
        let storage = self.backend.shutdown(deadline).await;
        match (stats, storage) {
            (Ok(stats), Ok(storage)) => Ok(StorageServiceFlushSummary {
                stats,
                storage,
                detail: SqliteResolveDetailFlushSummary::default(),
            }),
            (Err(stats), Err(backend)) => {
                Err(StorageServiceError::StatsAndBackend { stats, backend })
            }
            (Err(stats), Ok(_)) => Err(StorageServiceError::Stats(stats)),
            (Ok(_), Err(backend)) => Err(StorageServiceError::Backend(backend)),
        }
    }
}

impl StorageRuntime {
    /// 按已完成校验的配置打开业务数据库并组装 stats/detail writer。
    pub async fn open(
        config: &ResolvedConfig,
        deadline: Deadline,
    ) -> Result<Self, StorageRuntimeBuildError> {
        if !matches!(config.database.kind, DatabaseType::Sqlite) {
            return Err(StorageRuntimeBuildError::DatabaseType);
        }
        let backend = Arc::new(
            if config.version == crate::config::contract::CONFIG_VERSION {
                SqliteStorageBackend::connect_with_deadline(config.database.path.clone(), deadline)
                    .await
                    .map_err(StorageRuntimeBuildError::Connect)?
            } else {
                SqliteStorageBackend::connect_with_deadline(config.database.path.clone(), deadline)
                    .await
                    .map_err(StorageRuntimeBuildError::Connect)?
            },
        );
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline)
            .await
            .map_err(StorageRuntimeBuildError::Migration)?;
        backend
            .startup_write_probe(deadline)
            .await
            .map_err(StorageRuntimeBuildError::WriteProbe)?;
        if deadline.is_expired(std::time::Instant::now()) {
            return Err(StorageRuntimeBuildError::Connect(
                SqliteStorageBackendBuildError::Timeout,
            ));
        }

        let retention_bootstrap = backend
            .retention_bootstrap(deadline)
            .await
            .map_err(StorageRuntimeBuildError::RetentionBootstrap)?;
        let stats_worker = Arc::new(StatsPersistenceWorker::with_next_batch_id(
            backend.clone(),
            retention_bootstrap.next_stats_batch_id,
        ));
        let service =
            StorageService::new(backend.clone()).with_stats_worker(Arc::clone(&stats_worker));
        #[cfg(test)]
        let backend_for_test = Arc::clone(&backend);
        let detail_store = Arc::new(
            DetailShardStore::new(
                config.database.records_path.clone(),
                vec![
                    config.database.path.clone(),
                    config.dns.cache.persistence_path.clone(),
                ],
                DEFAULT_MAX_ACTIVE_DETAIL_SHARDS,
            )
            .map_err(StorageRuntimeBuildError::DetailStore)?,
        );
        if let Some(state) = retention_bootstrap.state {
            detail_store.publish_retired_before(state.retired_before_day_utc);
        }
        let retention = Arc::new(RetentionCoordinator::new(
            Arc::clone(&backend),
            Arc::clone(&stats_worker),
            Arc::clone(&detail_store),
        ));
        let mut detail_cancellation = None;
        let mut detail_task = None;
        let detail_writer = if config.dns.resolve_log.enable {
            let (writer, worker) = ShardedResolveDetailWriter::channel(
                Arc::clone(&detail_store),
                DEFAULT_RESOLVE_LOG_QUEUE_CAPACITY,
                DEFAULT_RESOLVE_LOG_BATCH_SIZE,
            )
            .map_err(StorageRuntimeBuildError::DetailChannel)?;
            let cancellation = Cancellation::new();
            detail_task = Some(tokio::spawn(worker.run_until_stopped(
                cancellation.clone(),
                DEFAULT_STORAGE_FLUSH_INTERVAL,
                DEFAULT_STORAGE_OPERATION_TIMEOUT,
            )));
            detail_cancellation = Some(cancellation);
            Some(writer)
        } else {
            None
        };
        let retention_cancellation = Cancellation::new();
        let retention_policy = RetentionPolicy::new(
            config.statistics.retention_days,
            config.statistics.retention_grace_days,
            config.statistics.retention_reference_size_bytes,
        )
        .map_err(StorageRuntimeBuildError::RetentionPolicy)?;
        let retention_task = tokio::spawn(
            RetentionScheduler::new(Arc::clone(&retention), retention_policy)
                .run_until_stopped(retention_cancellation.clone()),
        );

        Ok(Self {
            service,
            #[cfg(test)]
            backend_for_test,
            detail_store,
            retention,
            retention_cancellation: Some(retention_cancellation),
            retention_task: Some(retention_task),
            detail_writer,
            resolution_metrics: Arc::new(crate::resolution::ResolutionPipelineMetrics::default()),
            detail_cancellation,
            detail_task,
        })
    }

    pub fn stats_worker(&self) -> Arc<StatsPersistenceWorker> {
        self.service
            .stats_worker()
            .expect("storage runtime always owns a stats worker")
    }

    pub fn stats_recorder(&self) -> Arc<dyn StatsRecorder> {
        self.service
            .stats_recorder()
            .expect("storage runtime always owns a stats recorder")
    }

    pub fn retention_coordinator(&self) -> Arc<RetentionCoordinator> {
        Arc::clone(&self.retention)
    }

    pub(crate) fn detail_writer(&self) -> Option<ShardedResolveDetailWriter> {
        self.detail_writer.clone()
    }

    pub(crate) fn detail_store(&self) -> Arc<DetailShardStore> {
        Arc::clone(&self.detail_store)
    }

    pub(crate) fn resolution_metrics(&self) -> Arc<crate::resolution::ResolutionPipelineMetrics> {
        Arc::clone(&self.resolution_metrics)
    }

    /// 提交当前统计和存储批次并返回完整摘要。
    pub async fn flush(
        &mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
        self.service.flush(deadline).await
    }

    /// 停止详情的新输入，先保存统计，再排空独立分片并关闭两个存储 owner。
    pub async fn shutdown(
        &mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
        self.detail_writer = None;
        if let Some(cancellation) = self.retention_cancellation.take() {
            cancellation.cancel(CancelReason::Shutdown);
        }
        if let Some(cancellation) = self.detail_cancellation.take() {
            cancellation.cancel(CancelReason::Shutdown);
        }
        let retention_owner = match self.retention_task.take() {
            Some(mut task) => {
                match tokio::time::timeout(deadline.remaining(std::time::Instant::now()), &mut task)
                    .await
                {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(_)) => Err(PortError::new(
                        crate::ports::PortErrorClass::Internal,
                        "retention.scheduler",
                    )),
                    Err(_) => {
                        task.abort();
                        let _ = task.await;
                        Err(PortError::new(
                            crate::ports::PortErrorClass::Timeout,
                            "retention.scheduler_shutdown",
                        ))
                    }
                }
            }
            None => Ok(()),
        };
        let detail_owner = match self.detail_task.take() {
            Some(mut task) => {
                match tokio::time::timeout(deadline.remaining(std::time::Instant::now()), &mut task)
                    .await
                {
                    Ok(Ok(Ok((worker, summary)))) => Ok(Some((worker, summary))),
                    Ok(Ok(Err(error))) => Err(error),
                    Ok(Err(_)) => Err(PortError::new(
                        crate::ports::PortErrorClass::Internal,
                        "detail_shard.worker",
                    )),
                    Err(_) => {
                        task.abort();
                        // 等待 task 释放当前 lease；底层 transaction 由连接关闭回滚。
                        let _ = task.await;
                        Err(PortError::new(
                            crate::ports::PortErrorClass::Timeout,
                            "detail_shard.shutdown",
                        ))
                    }
                }
            }
            None => Ok(None),
        };
        let service = self.service.shutdown(deadline).await;
        let service = match (retention_owner, service) {
            (Ok(()), service) => service,
            (Err(error), Ok(_)) => Err(StorageServiceError::Backend(error)),
            // service 自身错误包含更精确的 stats/backend 状态；retention owner 错误仍由状态表保留。
            (Err(_), Err(service)) => Err(service),
        };
        let detail = match detail_owner {
            Ok(Some((worker, mut summary))) => match worker.shutdown(deadline).await {
                Ok(final_flush) => {
                    summary.flush.committed = summary
                        .flush
                        .committed
                        .saturating_add(final_flush.committed);
                    summary.flush.evicted =
                        summary.flush.evicted.saturating_add(final_flush.evicted);
                    summary.flush.dropped =
                        summary.flush.dropped.saturating_add(final_flush.dropped);
                    Ok(summary)
                }
                Err(error) => Err(error),
            },
            Ok(None) => Ok(SqliteResolveDetailRunSummary::default()),
            Err(error) => Err(error),
        };
        let detail = match (detail, self.detail_store.shutdown(deadline).await) {
            (Ok(summary), Ok(())) => Ok(summary),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        };
        match (detail, service) {
            (Ok(detail), Ok(mut summary)) => {
                summary.detail.committed = summary
                    .detail
                    .committed
                    .saturating_add(detail.flush.committed);
                summary.detail.evicted =
                    summary.detail.evicted.saturating_add(detail.flush.evicted);
                summary.detail.dropped =
                    summary.detail.dropped.saturating_add(detail.flush.dropped);
                Ok(summary)
            }
            (Err(detail), Ok(_)) => Err(StorageServiceError::Detail(detail)),
            (Ok(_), Err(service)) => Err(service),
            (Err(detail), Err(StorageServiceError::Stats(stats))) => {
                Err(StorageServiceError::StatsAndDetail { stats, detail })
            }
            (Err(detail), Err(StorageServiceError::Backend(backend))) => {
                Err(StorageServiceError::Both { detail, backend })
            }
            (Err(detail), Err(StorageServiceError::StatsAndBackend { stats, backend })) => {
                Err(StorageServiceError::All {
                    stats,
                    detail,
                    backend,
                })
            }
            (Err(_), Err(service)) => Err(service),
        }
    }
}

impl std::fmt::Debug for StorageService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StorageService")
            .field("backend", &"StorageBackend")
            .field("has_stats_worker", &self.has_stats_worker())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use crate::config::{ConfigV2Loader, LoadOptions};
    use crate::dns::Deadline;
    use crate::ports::PortFuture;
    use crate::ports::storage::{
        ResolveEvent, SchemaVersion, StatsSource, StorageBackend, StorageFlushSummary,
        StorageHealth, StorageTransaction,
    };
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};

    use super::{StorageRuntime, StorageService, StorageServiceError};

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(1))
    }

    fn detail_record() -> crate::storage::ResolveDetailRecord {
        crate::storage::ResolveDetailRecord::from_event(ResolveEvent {
            occurred_at: SystemTime::now(),
            duration_millis: 8,
            dns_core_duration_micros: 250,
            request_digest: std::sync::Arc::from("request-digest"),
            listener_id: std::sync::Arc::from("udp-main"),
            route_id: None,
            client_id: None,
            client_ip: None,
            client_match_source: None,
            matched_client_id: None,
            client_bucket: None,
            strategy_id: None,
            upstream_id: None,
            upstream_member_id: None,
            upstream_used_id: None,
            matched_rule_source: None,
            matched_resource_id: None,
            matched_rule_ordinal: None,
            resource_version: None,
            transport: crate::dns::TransportClass::Datagram,
            qname: std::sync::Arc::from("example.test."),
            qtype: 1,
            qclass: 1,
            answers: Vec::new(),
            rcode: 0,
            cancellation_reason: None,
            outcome: OutcomeClass::Success,
            source: StatsSource::Upstream,
            cache_status: CacheStatus::Miss,
            runtime_revision: crate::dns::RuntimeRevision(1),
        })
        .expect("detail event must be valid")
    }

    #[tokio::test]
    async fn v2_runtime_initializes_writes_shuts_down_and_reopens_new_layout() {
        let root = std::path::PathBuf::from(crate::config::test_support::absolute_path(
            "v2-storage-runtime",
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source_path = root.join("input.yaml");
        let source = include_str!("../../tests/fixtures/config-v2.yaml")
            .replace("dns: {}", "dns:\n  resolve_log:\n    enable: true");
        std::fs::write(&source_path, source).unwrap();
        let config = ConfigV2Loader::default()
            .load_from_path(&source_path)
            .unwrap()
            .resolved;
        let operation_deadline = || Deadline::new(Instant::now() + Duration::from_secs(5));

        let mut runtime = StorageRuntime::open(&config, operation_deadline())
            .await
            .unwrap();
        assert_eq!(runtime.detail_store().root(), config.database.records_path);
        runtime
            .detail_writer()
            .unwrap()
            .try_write(detail_record())
            .unwrap();
        runtime.shutdown(operation_deadline()).await.unwrap();

        let marker_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&config.database.path)
                    .read_only(true),
            )
            .await
            .unwrap();
        let marker: String = sqlx::query_scalar(
            "SELECT kind FROM fluxdns_layout WHERE singleton = 1 AND layout_version = 1",
        )
        .fetch_one(&marker_pool)
        .await
        .unwrap();
        assert_eq!(marker, "statistics-v2");
        marker_pool.close().await;
        assert!(
            std::fs::read_dir(&config.database.records_path)
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|value| value == "sqlite3"))
        );

        let mut reopened = StorageRuntime::open(&config, operation_deadline())
            .await
            .unwrap();
        reopened.shutdown(operation_deadline()).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// V1-O03：真实 detail worker 已返回后模拟 owner join panic，不泄露 payload 或跳过统计。
    #[tokio::test]
    async fn contract_v1_detail_owner_panic_preserves_stats_and_safe_error() {
        let (source, work_path) = crate::config::test_support::portable_example();
        let config = ConfigV2Loader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .unwrap()
            .resolved;
        let mut runtime = StorageRuntime::open(config.as_ref(), deadline())
            .await
            .unwrap();
        let writer = runtime.detail_writer().unwrap();
        runtime
            .stats_worker()
            .record_request(20_260_905, Vec::new())
            .unwrap();
        let worker = runtime.detail_task.take().unwrap();
        runtime.detail_task = Some(tokio::spawn(async move {
            let _completed = worker.await.unwrap().unwrap();
            panic!("synthetic private detail panic payload");
        }));
        let error = tokio::time::timeout(Duration::from_secs(5), runtime.shutdown(deadline()))
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(&error, StorageServiceError::Detail(source)
            if source.operation() == "detail_shard.worker"
                && matches!(source.class(), crate::ports::PortErrorClass::Internal)));
        assert!(!format!("{error:?}").contains("private detail"));
        assert!(runtime.detail_task.is_none());
        assert!(writer.try_write(detail_record()).is_err());
        let reopened = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&config.database.path))
            .await
            .unwrap();
        let total: i64 = sqlx::query_scalar("SELECT SUM(total_requests) FROM stats_daily_total")
            .fetch_one(&reopened)
            .await
            .unwrap();
        assert_eq!(total, 1);
        reopened.close().await;
        drop((runtime, writer));
        std::fs::remove_dir_all(work_path).unwrap();
    }

    /// V4-S03：真实 SQLite 的当前事务不被统计抢占；超时与已提交数据分别断言。
    #[tokio::test]
    async fn contract_v4_sql_stages_share_shutdown_budget_and_reclaim_owner() {
        use std::sync::Arc;

        use crate::ports::testing::TestGate;
        use crate::storage::detail_shards::DetailSqlTestStage;

        for stage in [
            DetailSqlTestStage::BeforeSql,
            DetailSqlTestStage::BeforeCommit,
            DetailSqlTestStage::AfterCommit,
        ] {
            for expire in [false, true] {
                let (source, work_path) = crate::config::test_support::portable_example();
                let config = ConfigV2Loader::new(LoadOptions::default().without_snapshot())
                    .load_str(&source)
                    .unwrap()
                    .resolved;
                let mut runtime = StorageRuntime::open(config.as_ref(), deadline())
                    .await
                    .unwrap();
                let backend = runtime.backend_for_test.clone();
                let detail_store = runtime.detail_store();
                let stats = runtime.stats_worker();
                let writer = runtime.detail_writer().unwrap();
                let cancelled = runtime.detail_cancellation.as_ref().unwrap().clone();
                let gate = Arc::new(TestGate::new());
                detail_store.set_detail_test_gate(stage, gate.clone());
                stats.record_request(20_260_905, Vec::new()).unwrap();
                let record = detail_record();
                let detail_day = crate::storage::day_utc(record.occurred_at()).unwrap();
                for _ in 0..super::DEFAULT_RESOLVE_LOG_BATCH_SIZE {
                    writer.try_write(record.clone()).unwrap();
                }
                gate.wait_reached().await;
                let detail_path = detail_store.shard_path(detail_day).unwrap();
                let verification = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect_with(
                        sqlx::sqlite::SqliteConnectOptions::new()
                            .filename(&detail_path)
                            .read_only(true),
                    )
                    .await
                    .unwrap();
                let stats_verification = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect_with(
                        sqlx::sqlite::SqliteConnectOptions::new()
                            .filename(&config.database.path)
                            .read_only(true),
                    )
                    .await
                    .unwrap();
                let visible: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
                    .fetch_one(&verification)
                    .await
                    .unwrap();
                assert_eq!(
                    visible,
                    if stage == DetailSqlTestStage::AfterCommit {
                        128
                    } else {
                        0
                    },
                    "{stage:?}"
                );
                let budget = Deadline::new(
                    Instant::now()
                        + if expire {
                            Duration::from_millis(30)
                        } else {
                            Duration::from_secs(1)
                        },
                );
                let shutdown = tokio::spawn(async move {
                    let result = runtime.shutdown(budget).await;
                    (runtime, result)
                });
                tokio::time::timeout(Duration::from_secs(5), cancelled.cancelled())
                    .await
                    .unwrap();
                // 取消已送达但当前详情事务仍在结束；owner 尚未进入统计提交阶段。
                assert!(!shutdown.is_finished());
                assert_eq!(stats.pending_batch_count(), 0);
                if !expire {
                    gate.release();
                }
                let (runtime, result) = tokio::time::timeout(Duration::from_secs(5), shutdown)
                    .await
                    .expect("shutdown watchdog")
                    .unwrap();
                assert!(runtime.detail_task.is_none());
                assert!(runtime.detail_cancellation.is_none());
                assert!(writer.try_write(detail_record()).is_err());
                if expire {
                    let error = result.unwrap_err();
                    assert!(error.is_timeout(), "{stage:?}: {error:?}");
                    assert!(matches!(error, StorageServiceError::All { .. }));
                    assert!(budget.is_expired(Instant::now()));
                    assert_eq!(stats.pending_batch_count(), 1);
                    // 这里只是显式清理与幂等恢复，不把新预算算作原 shutdown 成功。
                    assert_eq!(stats.flush(deadline()).await.unwrap().events_committed, 1);
                    assert_eq!(stats.flush(deadline()).await.unwrap().events_committed, 0);
                    backend.shutdown(deadline()).await.unwrap();
                } else {
                    let summary = result.unwrap();
                    assert_eq!(summary.stats.events_committed, 1);
                    assert_eq!(summary.detail.committed, 128);
                }
                let details: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
                    .fetch_one(&verification)
                    .await
                    .unwrap();
                assert_eq!(
                    details,
                    if !expire || stage == DetailSqlTestStage::AfterCommit {
                        128
                    } else {
                        0
                    },
                    "{stage:?}, expire={expire}"
                );
                let total: i64 =
                    sqlx::query_scalar("SELECT SUM(total_requests) FROM stats_daily_total")
                        .fetch_one(&stats_verification)
                        .await
                        .unwrap();
                assert_eq!(total, 1);
                let legacy_details: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'resolve_log'")
                    .fetch_one(&stats_verification)
                    .await
                    .unwrap();
                assert_eq!(legacy_details, 0);
                let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
                    .fetch_one(&verification)
                    .await
                    .unwrap();
                assert_eq!(integrity, "ok");
                verification.close().await;
                stats_verification.close().await;
                drop((runtime, backend, detail_store, stats, writer));
                std::fs::remove_dir_all(work_path).unwrap();
            }
        }
    }

    struct RecordingBackend {
        flushes: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                flushes: AtomicUsize::new(0),
                shutdowns: AtomicUsize::new(0),
            }
        }
    }

    impl StorageBackend for RecordingBackend {
        fn migrate(
            &self,
            target: SchemaVersion,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<SchemaVersion, crate::ports::PortError>> {
            Box::pin(async move { Ok(target) })
        }

        fn execute(
            &self,
            _transaction: StorageTransaction,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<(), crate::ports::PortError>> {
            Box::pin(async { Ok(()) })
        }

        fn health_probe(
            &self,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<StorageHealth, crate::ports::PortError>> {
            Box::pin(async { Ok(StorageHealth::Healthy) })
        }

        fn checkpoint(
            &self,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<(), crate::ports::PortError>> {
            Box::pin(async { Ok(()) })
        }

        fn flush(
            &self,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<StorageFlushSummary, crate::ports::PortError>> {
            self.flushes.fetch_add(1, Ordering::AcqRel);
            Box::pin(async { Ok(StorageFlushSummary::default()) })
        }

        fn shutdown(
            &self,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<StorageFlushSummary, crate::ports::PortError>> {
            self.shutdowns.fetch_add(1, Ordering::AcqRel);
            Box::pin(async { Ok(StorageFlushSummary::default()) })
        }
    }

    #[tokio::test]
    async fn flush_and_shutdown_delegate_to_backend_once() {
        let backend = std::sync::Arc::new(RecordingBackend::new());
        let service_backend = std::sync::Arc::clone(&backend);
        let mut service = StorageService::new(backend);
        let deadline = Deadline::new(Instant::now() + Duration::from_secs(1));

        assert_eq!(service.flush(deadline).await.unwrap().detail.committed, 0);
        let summary = service.shutdown(deadline).await.unwrap();

        assert_eq!(
            summary.detail,
            super::SqliteResolveDetailFlushSummary::default()
        );
        assert_eq!(service_backend.flushes.load(Ordering::Acquire), 1);
        assert_eq!(service_backend.shutdowns.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn stats_worker_is_flushed_before_backend_shutdown() {
        let backend = std::sync::Arc::new(crate::storage::InMemoryStorageBackend::new());
        backend
            .migrate(crate::storage::STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let worker =
            std::sync::Arc::new(crate::storage::StatsPersistenceWorker::new(backend.clone()));
        worker
            .record_request(
                20_260_902,
                vec![crate::ports::storage::StatsDimension::transport(
                    crate::dns::TransportClass::Datagram,
                )],
            )
            .unwrap();
        let mut service = StorageService::new(backend.clone()).with_stats_worker(worker.clone());
        assert!(service.has_stats_worker());
        assert!(service.stats_recorder().is_some());

        let flush = service.flush(deadline()).await.unwrap();
        assert_eq!(flush.stats.events_committed, 1);
        assert_eq!(backend.total_for_day(20_260_902), 1);

        worker
            .record_request(20_260_902, Vec::new())
            .expect("worker remains usable until shutdown");
        let shutdown = service.shutdown(deadline()).await.unwrap();
        assert_eq!(shutdown.stats.events_committed, 1);
        assert_eq!(backend.total_for_day(20_260_902), 2);
    }

    #[tokio::test]
    async fn storage_runtime_separates_stats_and_writes_all_details_to_day_shards() {
        let (source, work_path) = crate::config::test_support::portable_example();
        let config = ConfigV2Loader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("storage runtime fixture must be valid")
            .resolved;
        let mut runtime = StorageRuntime::open(config.as_ref(), deadline())
            .await
            .expect("storage runtime must open configured sqlite");
        let _stats_recorder = runtime.stats_recorder();
        assert_eq!(runtime.stats_worker().pending_batch_count(), 0);
        let writer = runtime
            .detail_writer()
            .expect("resolved fixture enables detail writer");
        let record = detail_record();
        let day = crate::storage::day_utc(record.occurred_at()).unwrap();
        let detail_store = runtime.detail_store();
        let lease = detail_store.acquire_write(day, deadline()).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_record_delete BEFORE DELETE ON resolve_log \
             BEGIN SELECT RAISE(ABORT, 'detail writer must not delete'); END",
        )
        .execute(lease.pool())
        .await
        .unwrap();
        lease.close(deadline()).await.unwrap();
        for _ in 0..300 {
            writer
                .try_write(record.clone())
                .expect("detail record must be accepted");
        }
        runtime
            .stats_worker()
            .record_request(20_260_905, Vec::new())
            .unwrap();
        let shutdown = runtime
            .shutdown(deadline())
            .await
            .expect("storage runtime shutdown must drain configured writers");
        assert_eq!(shutdown.stats.events_committed, 1);
        assert_eq!(shutdown.detail.committed, 300);
        assert!(writer.try_write(record).is_err());
        let detail_verification = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(detail_store.shard_path(day).unwrap())
                    .read_only(true),
            )
            .await
            .unwrap();
        let detail_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(&detail_verification)
            .await
            .unwrap();
        assert_eq!(detail_count, 300);
        detail_verification.close().await;
        let stats_verification = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&config.database.path)
                    .read_only(true),
            )
            .await
            .unwrap();
        let legacy_detail_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'resolve_log'",
        )
        .fetch_one(&stats_verification)
        .await
        .unwrap();
        assert_eq!(legacy_detail_count, 0);
        stats_verification.close().await;
        let _ = std::fs::remove_dir_all(work_path);
    }

    #[tokio::test]
    async fn storage_runtime_restores_persisted_retention_watermark() {
        let (source, work_path) = crate::config::test_support::portable_example();
        let config = ConfigV2Loader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("storage runtime fixture must be valid")
            .resolved;
        let reference_day = 20_710;
        let plan = crate::storage::RetentionPlan::calculate(
            crate::storage::RetentionPolicy::new(3, 0, 1 << 30).unwrap(),
            reference_day,
            0,
        )
        .unwrap();
        let mut runtime = StorageRuntime::open(config.as_ref(), deadline())
            .await
            .unwrap();
        assert!(runtime.retention_task.is_some());
        let state = runtime
            .retention_coordinator()
            .publish(plan, deadline())
            .await
            .unwrap();
        assert_eq!(state.retired_before_day_utc, reference_day - 2);
        assert_eq!(
            runtime.detail_store.retired_before(),
            Some(reference_day - 2)
        );
        runtime.shutdown(deadline()).await.unwrap();
        assert!(runtime.retention_task.is_none());
        drop(runtime);

        let mut reopened = StorageRuntime::open(config.as_ref(), deadline())
            .await
            .unwrap();
        assert_eq!(
            reopened.detail_store.retired_before(),
            Some(reference_day - 2)
        );
        assert!(reopened.retention_task.is_some());
        reopened.shutdown(deadline()).await.unwrap();
        assert!(reopened.retention_task.is_none());
        drop(reopened);
        std::fs::remove_dir_all(work_path).unwrap();
    }

    #[test]
    fn detail_error_variant_keeps_safe_error_boundary() {
        let error = StorageServiceError::Detail(crate::ports::PortError::new(
            crate::ports::PortErrorClass::Unavailable,
            "detail",
        ));
        assert!(error.to_string().contains("resolve detail worker"));
    }

    #[test]
    fn pending_limit_error_is_classified_as_fatal() {
        let error = StorageServiceError::Stats(
            crate::storage::StatsPersistenceError::PendingLimitExceeded(Box::new(
                crate::storage::StatsPendingLimit {
                    pending_batches: 64,
                    pending_events: 64,
                    active_events: 1,
                    max_pending_batches: 64,
                    max_pending_events: 65_536,
                },
            )),
        );
        assert!(error.is_fatal());
        assert!(
            !StorageServiceError::Backend(crate::ports::PortError::new(
                crate::ports::PortErrorClass::Unavailable,
                "backend",
            ))
            .is_fatal()
        );
    }
}
