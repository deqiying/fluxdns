//! Storage writer 的统一 flush/shutdown 生命周期边界。

use std::sync::Arc;
use std::time::Duration;

use crate::config::model::DatabaseType;
use crate::config::resolve::ResolvedConfig;
use crate::dns::Deadline;
use crate::ports::PortError;
use crate::ports::storage::{ResolveEventSink, StatsRecorder, StorageBackend, StorageFlushSummary};

use super::{
    ResolveLogBuildError, ResolveLogShutdownSummary, ResolveLogWriter, STORAGE_SCHEMA_VERSION,
    SqliteResolveDetailFlushSummary, SqliteResolveDetailLimits, SqliteResolveDetailWorker,
    SqliteResolveDetailWriter, SqliteResolveDetailWriterBuildError, SqliteStorageBackend,
    SqliteStorageBackendBuildError, StatsPersistenceError, StatsPersistenceFlushSummary,
    StatsPersistenceWorker,
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
    /// 详情前端队列的提交、丢弃和 sink 失败摘要。
    pub resolve_log: ResolveLogShutdownSummary,
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
    resolve_log: Option<Arc<ResolveLogWriter<SqliteResolveDetailWriter>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageRuntimeBuildError {
    #[error("unsupported storage database type")]
    DatabaseType,
    #[error("sqlite storage could not be opened: {0}")]
    Connect(#[source] SqliteStorageBackendBuildError),
    #[error("sqlite storage migration failed: {0}")]
    Migration(#[source] PortError),
    #[error("resolve detail limits are invalid: {0}")]
    DetailLimits(#[source] SqliteResolveDetailWriterBuildError),
    #[error("resolve detail channel could not be created: {0}")]
    DetailChannel(#[source] SqliteResolveDetailWriterBuildError),
    #[error("resolve detail writer could not be created: {0}")]
    DetailWriter(#[source] ResolveLogBuildError),
}

/// 业务 Storage 的统一 flush/shutdown facade。
///
/// stats 与 detail worker 都必须在 backend 关闭前完成提交；shutdown 会先完成这两个
/// writer 的边界，再关闭 backend，避免 worker 使用已关闭的 pool。
pub struct StorageService {
    backend: Arc<dyn StorageBackend>,
    stats_worker: Option<Arc<StatsPersistenceWorker>>,
    detail_worker: Option<SqliteResolveDetailWorker>,
}

impl StorageService {
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self {
            backend,
            stats_worker: None,
            detail_worker: None,
        }
    }

    pub fn with_stats_worker(mut self, worker: Arc<StatsPersistenceWorker>) -> Self {
        self.stats_worker = Some(worker);
        self
    }

    pub fn with_detail_worker(mut self, worker: SqliteResolveDetailWorker) -> Self {
        self.detail_worker = Some(worker);
        self
    }

    pub fn has_stats_worker(&self) -> bool {
        self.stats_worker.is_some()
    }

    pub fn has_detail_worker(&self) -> bool {
        self.detail_worker.is_some()
    }

    /// 返回可由请求热路径共享的同步 stats recorder。
    pub fn stats_recorder(&self) -> Option<Arc<dyn StatsRecorder>> {
        self.stats_worker
            .as_ref()
            .map(|worker| Arc::clone(worker) as Arc<dyn StatsRecorder>)
    }

    pub fn stats_worker(&self) -> Option<Arc<StatsPersistenceWorker>> {
        self.stats_worker.as_ref().map(Arc::clone)
    }

    /// 先提交 stats，再 checkpoint backend，最后提交当前 detail batch。
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
        let detail = match self.detail_worker.as_mut() {
            Some(worker) => worker
                .flush(deadline)
                .await
                .map_err(StorageServiceError::Detail)?,
            None => SqliteResolveDetailFlushSummary::default(),
        };
        Ok(StorageServiceFlushSummary {
            stats,
            storage,
            resolve_log: ResolveLogShutdownSummary::default(),
            detail,
        })
    }

    /// 在同一 deadline 内提交 stats、drain detail，再关闭 backend。
    pub async fn shutdown(
        &mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
        let stats = match self.stats_worker.take() {
            Some(worker) => worker.flush(deadline).await,
            None => Ok(StatsPersistenceFlushSummary::default()),
        };
        let detail = match self.detail_worker.take() {
            Some(worker) => worker.shutdown(deadline).await,
            None => Ok(SqliteResolveDetailFlushSummary::default()),
        };
        let storage = self.backend.shutdown(deadline).await;

        match (stats, detail, storage) {
            (Ok(stats), Ok(detail), Ok(storage)) => Ok(StorageServiceFlushSummary {
                stats,
                storage,
                resolve_log: ResolveLogShutdownSummary::default(),
                detail,
            }),
            (Err(stats), Err(detail), Err(backend)) => Err(StorageServiceError::All {
                stats,
                detail,
                backend,
            }),
            (Err(stats), Err(detail), Ok(_)) => {
                Err(StorageServiceError::StatsAndDetail { stats, detail })
            }
            (Err(stats), Ok(_), Err(backend)) => {
                Err(StorageServiceError::StatsAndBackend { stats, backend })
            }
            (Err(stats), Ok(_), Ok(_)) => Err(StorageServiceError::Stats(stats)),
            (Ok(_), Err(detail), Ok(_)) => Err(StorageServiceError::Detail(detail)),
            (Ok(_), Ok(_), Err(backend)) => Err(StorageServiceError::Backend(backend)),
            (Ok(_), Err(detail), Err(backend)) => {
                Err(StorageServiceError::Both { detail, backend })
            }
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
            SqliteStorageBackend::connect(config.database.path.clone())
                .await
                .map_err(StorageRuntimeBuildError::Connect)?,
        );
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline)
            .await
            .map_err(StorageRuntimeBuildError::Migration)?;

        let stats_worker = Arc::new(StatsPersistenceWorker::new(backend.clone()));
        let mut service = StorageService::new(backend.clone()).with_stats_worker(stats_worker);
        let resolve_log = if config.dns.resolve_log.enable {
            let limits = SqliteResolveDetailLimits::new(
                config.dns.resolve_log.eviction_threshold_records,
                config.dns.resolve_log.max_records,
                config.dns.resolve_log.max_record_age,
            )
            .map_err(StorageRuntimeBuildError::DetailLimits)?;
            let (writer, worker) = SqliteResolveDetailWriter::channel_with_limits(
                backend,
                DEFAULT_RESOLVE_LOG_QUEUE_CAPACITY,
                DEFAULT_RESOLVE_LOG_BATCH_SIZE,
                limits,
            )
            .map_err(StorageRuntimeBuildError::DetailChannel)?;
            let sink = Arc::new(
                ResolveLogWriter::new(true, DEFAULT_RESOLVE_LOG_QUEUE_CAPACITY, writer)
                    .map_err(StorageRuntimeBuildError::DetailWriter)?,
            );
            service = service.with_detail_worker(worker);
            Some(sink)
        } else {
            None
        };

        Ok(Self {
            service,
            resolve_log,
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

    pub fn resolve_event_sink(&self) -> Option<Arc<dyn ResolveEventSink>> {
        self.resolve_log
            .as_ref()
            .map(|sink| Arc::clone(sink) as Arc<dyn ResolveEventSink>)
    }

    /// 将详情前端队列交给 worker 后，统一提交当前存储批次并返回完整摘要。
    pub async fn flush(
        &mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
        let resolve_log =
            self.resolve_log
                .as_ref()
                .map_or_else(ResolveLogShutdownSummary::default, |sink| {
                    ResolveLogShutdownSummary {
                        flush: sink.flush(),
                        discarded_pending: 0,
                    }
                });
        let mut summary = self.service.flush(deadline).await?;
        summary.resolve_log = resolve_log;
        Ok(summary)
    }

    /// 停止详情前端接收并返回最终丢弃数，再按既定顺序关闭存储 worker。
    pub async fn shutdown(
        &mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
        let resolve_log = self
            .resolve_log
            .as_ref()
            .map_or_else(ResolveLogShutdownSummary::default, |sink| sink.shutdown());
        let mut summary = self.service.shutdown(deadline).await?;
        summary.resolve_log = resolve_log;
        Ok(summary)
    }
}

impl std::fmt::Debug for StorageService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StorageService")
            .field("backend", &"StorageBackend")
            .field("has_stats_worker", &self.has_stats_worker())
            .field("has_detail_worker", &self.has_detail_worker())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::Deadline;
    use crate::ports::PortFuture;
    use crate::ports::storage::{
        ResolveEvent, ResolveEventDisposition, SchemaVersion, StatsSource, StorageBackend,
        StorageFlushSummary, StorageHealth, StorageTransaction,
    };
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};

    use super::{StorageRuntime, StorageService, StorageServiceError};

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(1))
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
    async fn storage_runtime_opens_from_resolved_config_with_detail_sink() {
        let (source, work_path) = crate::config::test_support::portable_example();
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("storage runtime fixture must be valid")
            .resolved;
        let mut runtime = StorageRuntime::open(config.as_ref(), deadline())
            .await
            .expect("storage runtime must open configured sqlite");

        let _stats_recorder = runtime.stats_recorder();
        assert_eq!(runtime.stats_worker().pending_batch_count(), 0);
        let sink = runtime
            .resolve_event_sink()
            .expect("resolved fixture enables detail sink");
        let disposition = sink
            .try_record(ResolveEvent {
                occurred_at: SystemTime::now(),
                duration_started_at: Instant::now(),
                request_digest: std::sync::Arc::from("request-digest"),
                listener_id: std::sync::Arc::from("udp-main"),
                route_id: None,
                client_bucket: None,
                strategy_id: None,
                upstream_id: None,
                upstream_member_id: None,
                matched_rule_source: None,
                matched_resource_id: None,
                matched_rule_ordinal: None,
                resource_version: None,
                transport: crate::dns::TransportClass::Datagram,
                qname: std::sync::Arc::from("example.test."),
                qtype: 1,
                qclass: 1,
                rcode: 0,
                cancellation_reason: None,
                outcome: OutcomeClass::Success,
                source: StatsSource::Upstream,
                cache_status: CacheStatus::Miss,
                runtime_revision: crate::dns::RuntimeRevision(1),
            })
            .expect("detail event must be accepted");
        assert_eq!(disposition, ResolveEventDisposition::Accepted);

        let flush = runtime.flush(deadline()).await.unwrap();
        assert_eq!(flush.resolve_log.flush.committed, 1);
        assert_eq!(flush.resolve_log.discarded_pending, 0);
        assert_eq!(flush.detail.committed, 1);
        let shutdown = runtime
            .shutdown(deadline())
            .await
            .expect("storage runtime shutdown must drain configured writers");
        assert_eq!(shutdown.resolve_log.discarded_pending, 0);
        let _ = std::fs::remove_dir_all(work_path);
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
