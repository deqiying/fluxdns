//! Storage writer 的统一 flush/shutdown 生命周期边界。

use std::sync::Arc;

use crate::dns::Deadline;
use crate::ports::PortError;
use crate::ports::storage::{StatsRecorder, StorageBackend, StorageFlushSummary};

use super::{
    SqliteResolveDetailFlushSummary, SqliteResolveDetailWorker, StatsPersistenceError,
    StatsPersistenceFlushSummary, StatsPersistenceWorker,
};

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
            detail,
        })
    }

    /// 在同一 deadline 内提交 stats、drain detail，再关闭 backend。
    pub async fn shutdown(
        mut self,
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
    use std::time::{Duration, Instant};

    use crate::dns::Deadline;
    use crate::ports::PortFuture;
    use crate::ports::storage::{
        SchemaVersion, StorageBackend, StorageFlushSummary, StorageHealth, StorageTransaction,
    };

    use super::{StorageService, StorageServiceError};

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

    #[test]
    fn detail_error_variant_keeps_safe_error_boundary() {
        let error = StorageServiceError::Detail(crate::ports::PortError::new(
            crate::ports::PortErrorClass::Unavailable,
            "detail",
        ));
        assert!(error.to_string().contains("resolve detail worker"));
    }
}
