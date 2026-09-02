//! Storage writer 的统一 flush/shutdown 生命周期边界。

use std::sync::Arc;

use crate::dns::Deadline;
use crate::ports::PortError;
use crate::ports::storage::{StorageBackend, StorageFlushSummary};

use super::{SqliteResolveDetailFlushSummary, SqliteResolveDetailWorker};

/// Storage backend 与 resolve detail worker 的一次生命周期汇总。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageServiceFlushSummary {
    pub storage: StorageFlushSummary,
    pub detail: SqliteResolveDetailFlushSummary,
}

/// 统一 writer 生命周期中的失败；shutdown 会尽力执行两个子边界后再返回。
#[derive(Debug, thiserror::Error)]
pub enum StorageServiceError {
    #[error("storage backend operation failed: {0}")]
    Backend(#[source] PortError),
    #[error("resolve detail worker operation failed: {0}")]
    Detail(#[source] PortError),
    #[error("storage backend and resolve detail worker operations failed")]
    Both {
        detail: PortError,
        backend: PortError,
    },
}

/// 业务 Storage 的统一 flush/shutdown facade。
///
/// backend 必须先保持可用，detail worker 才能在同一 deadline 内执行提交；
/// shutdown 会先 drain detail，再关闭 backend，避免 worker 使用已关闭的 pool。
pub struct StorageService {
    backend: Arc<dyn StorageBackend>,
    detail_worker: Option<SqliteResolveDetailWorker>,
}

impl StorageService {
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self {
            backend,
            detail_worker: None,
        }
    }

    pub fn with_detail_worker(mut self, worker: SqliteResolveDetailWorker) -> Self {
        self.detail_worker = Some(worker);
        self
    }

    pub fn has_detail_worker(&self) -> bool {
        self.detail_worker.is_some()
    }

    /// 先 checkpoint backend，再提交当前 detail batch。
    pub async fn flush(
        &mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
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
        Ok(StorageServiceFlushSummary { storage, detail })
    }

    /// 在同一 deadline 内 drain detail，再关闭 backend。
    pub async fn shutdown(
        mut self,
        deadline: Deadline,
    ) -> Result<StorageServiceFlushSummary, StorageServiceError> {
        let detail = match self.detail_worker.take() {
            Some(worker) => worker
                .shutdown(deadline)
                .await
                .map_err(StorageServiceError::Detail),
            None => Ok(SqliteResolveDetailFlushSummary::default()),
        };
        let storage = self
            .backend
            .shutdown(deadline)
            .await
            .map_err(StorageServiceError::Backend);

        match (detail, storage) {
            (Ok(detail), Ok(storage)) => Ok(StorageServiceFlushSummary { storage, detail }),
            (
                Err(StorageServiceError::Detail(detail)),
                Err(StorageServiceError::Backend(backend)),
            ) => Err(StorageServiceError::Both { detail, backend }),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }
}

impl std::fmt::Debug for StorageService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StorageService")
            .field("backend", &"StorageBackend")
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

    #[test]
    fn detail_error_variant_keeps_safe_error_boundary() {
        let error = StorageServiceError::Detail(crate::ports::PortError::new(
            crate::ports::PortErrorClass::Unavailable,
            "detail",
        ));
        assert!(error.to_string().contains("resolve detail worker"));
    }
}
