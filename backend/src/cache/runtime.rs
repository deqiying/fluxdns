//! Cache persistence 的有界后台写入生命周期。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::dns::Deadline;
use crate::ports::cache::{PersistentCacheBatch, PersistentCacheStore};
use crate::ports::{PortError, PortErrorClass};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CachePersistenceRuntimeBuildError {
    ZeroCapacity,
    ZeroOperationTimeout,
    MissingRuntime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CachePersistenceEnqueueError {
    Full,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachePersistenceRunSummary {
    pub persisted_batches: u64,
    pub failed_batches: u64,
    pub dropped_batches: u64,
    pub capacity_removed: u64,
}

#[derive(Clone)]
pub struct CachePersistenceWriter {
    sender: mpsc::Sender<PersistenceCommand>,
    dropped_batches: Arc<AtomicU64>,
}

pub struct CachePersistenceRuntime {
    writer: CachePersistenceWriter,
    task: std::sync::Mutex<Option<JoinHandle<Result<CachePersistenceRunSummary, PortError>>>>,
}

enum PersistenceCommand {
    Persist(PersistentCacheBatch),
    Shutdown,
}

impl fmt::Debug for CachePersistenceWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachePersistenceWriter")
            .field("remaining_capacity", &self.sender.capacity())
            .field(
                "dropped_batches",
                &self.dropped_batches.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl fmt::Debug for CachePersistenceRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachePersistenceRuntime")
            .field("writer", &self.writer)
            .field(
                "running",
                &self
                    .task
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some(),
            )
            .finish()
    }
}

impl CachePersistenceWriter {
    /// 非阻塞提交一个持久化批次；队列满或关闭时由调用方降级为仅保留内存缓存。
    pub fn enqueue(&self, batch: PersistentCacheBatch) -> Result<(), CachePersistenceEnqueueError> {
        self.sender
            .try_send(PersistenceCommand::Persist(batch))
            .map_err(|error| {
                self.dropped_batches.fetch_add(1, Ordering::Relaxed);
                match error {
                    mpsc::error::TrySendError::Full(_) => CachePersistenceEnqueueError::Full,
                    mpsc::error::TrySendError::Closed(_) => CachePersistenceEnqueueError::Closed,
                }
            })
    }
}

impl CachePersistenceRuntime {
    /// 启动单写者后台任务；所有 adapter I/O 都离开 DNS 请求路径执行。
    pub fn start(
        store: Arc<dyn PersistentCacheStore>,
        capacity: usize,
        operation_timeout: Duration,
    ) -> Result<Self, CachePersistenceRuntimeBuildError> {
        if capacity == 0 {
            return Err(CachePersistenceRuntimeBuildError::ZeroCapacity);
        }
        if operation_timeout.is_zero() {
            return Err(CachePersistenceRuntimeBuildError::ZeroOperationTimeout);
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| CachePersistenceRuntimeBuildError::MissingRuntime)?;
        let (sender, receiver) = mpsc::channel(capacity);
        let dropped_batches = Arc::new(AtomicU64::new(0));
        let writer = CachePersistenceWriter {
            sender,
            dropped_batches: Arc::clone(&dropped_batches),
        };
        let task = runtime.spawn(run_worker(
            store,
            receiver,
            dropped_batches,
            operation_timeout,
        ));
        Ok(Self {
            writer,
            task: std::sync::Mutex::new(Some(task)),
        })
    }

    /// 返回可克隆的非阻塞写入端。
    pub fn writer(&self) -> CachePersistenceWriter {
        self.writer.clone()
    }

    /// 排空已入队批次并在 deadline 内关闭 persistence adapter。
    pub async fn shutdown(
        &self,
        deadline: Deadline,
    ) -> Result<CachePersistenceRunSummary, PortError> {
        if deadline.is_expired(Instant::now()) {
            return Err(timeout("cache_persistence.shutdown"));
        }
        let task = self
            .task
            .lock()
            .map_err(|_| PortError::new(PortErrorClass::Internal, "cache_persistence.shutdown"))?
            .take()
            .ok_or_else(|| unavailable("cache_persistence.shutdown"))?;
        let abort = task.abort_handle();
        let sender = self.writer.sender.clone();
        let remaining = deadline.remaining(Instant::now());
        match tokio::time::timeout(remaining, async move {
            let sent = sender.send(PersistenceCommand::Shutdown).await;
            let result = task.await.map_err(|_| {
                PortError::new(PortErrorClass::Internal, "cache_persistence.worker")
            })?;
            if sent.is_err() {
                return Err(unavailable("cache_persistence.shutdown"));
            }
            result
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                abort.abort();
                Err(timeout("cache_persistence.shutdown"))
            }
        }
    }
}

async fn run_worker(
    store: Arc<dyn PersistentCacheStore>,
    mut receiver: mpsc::Receiver<PersistenceCommand>,
    dropped_batches: Arc<AtomicU64>,
    operation_timeout: Duration,
) -> Result<CachePersistenceRunSummary, PortError> {
    let mut summary = CachePersistenceRunSummary::default();
    while let Some(command) = receiver.recv().await {
        match command {
            PersistenceCommand::Persist(batch) => {
                let deadline = Deadline::new(Instant::now() + operation_timeout);
                match store.persist(batch, deadline).await {
                    Ok(()) => {
                        summary.persisted_batches = summary.persisted_batches.saturating_add(1)
                    }
                    Err(_) => summary.failed_batches = summary.failed_batches.saturating_add(1),
                }
            }
            PersistenceCommand::Shutdown => break,
        }
    }
    summary.dropped_batches = dropped_batches.load(Ordering::Relaxed);
    summary.capacity_removed = store
        .maintain_capacity(Deadline::new(Instant::now() + operation_timeout))
        .await
        .unwrap_or_default();
    store
        .shutdown(Deadline::new(Instant::now() + operation_timeout))
        .await?;
    Ok(summary)
}

fn timeout(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Timeout, operation)
}

fn unavailable(operation: &'static str) -> PortError {
    PortError::new(PortErrorClass::Unavailable, operation)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{
        CachePersistenceRunSummary, CachePersistenceRuntime, CachePersistenceRuntimeBuildError,
    };
    use crate::dns::Deadline;
    use crate::ports::cache::{CacheRecoverySummary, PersistentCacheBatch, PersistentCacheStore};
    use crate::ports::{PortError, PortFuture};

    #[derive(Default)]
    struct FakePersistentStore {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        batches: usize,
        fail_next: bool,
        shutdown: bool,
    }

    impl PersistentCacheStore for FakePersistentStore {
        fn recover(
            &self,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<(PersistentCacheBatch, CacheRecoverySummary), PortError>>
        {
            Box::pin(async {
                Ok((
                    PersistentCacheBatch {
                        records: Vec::new(),
                    },
                    CacheRecoverySummary::default(),
                ))
            })
        }

        fn persist(
            &self,
            _batch: PersistentCacheBatch,
            _deadline: Deadline,
        ) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                if state.fail_next {
                    state.fail_next = false;
                    return Err(super::unavailable("fake_cache.persist"));
                }
                state.batches += 1;
                Ok(())
            })
        }

        fn maintain_capacity(&self, _deadline: Deadline) -> PortFuture<'_, Result<u64, PortError>> {
            Box::pin(async { Ok(0) })
        }

        fn shutdown(&self, _deadline: Deadline) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async move {
                self.state.lock().unwrap().shutdown = true;
                Ok(())
            })
        }
    }

    fn empty_batch() -> PersistentCacheBatch {
        PersistentCacheBatch {
            records: Vec::new(),
        }
    }

    #[test]
    fn rejects_invalid_runtime_limits() {
        let store = Arc::new(FakePersistentStore::default());
        assert!(matches!(
            CachePersistenceRuntime::start(store.clone(), 0, Duration::from_secs(1)),
            Err(CachePersistenceRuntimeBuildError::ZeroCapacity)
        ));
        assert!(matches!(
            CachePersistenceRuntime::start(store.clone(), 1, Duration::ZERO),
            Err(CachePersistenceRuntimeBuildError::ZeroOperationTimeout)
        ));
        assert!(matches!(
            CachePersistenceRuntime::start(store, 1, Duration::from_secs(1)),
            Err(CachePersistenceRuntimeBuildError::MissingRuntime)
        ));
    }

    #[tokio::test]
    async fn worker_continues_after_failed_batch_and_flushes_before_shutdown() {
        let store = Arc::new(FakePersistentStore::default());
        store.state.lock().unwrap().fail_next = true;
        let runtime =
            CachePersistenceRuntime::start(store.clone(), 4, Duration::from_secs(1)).unwrap();
        let writer = runtime.writer();
        writer.enqueue(empty_batch()).unwrap();
        writer.enqueue(empty_batch()).unwrap();

        let summary = runtime
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(2)))
            .await
            .unwrap();

        assert_eq!(
            summary,
            CachePersistenceRunSummary {
                persisted_batches: 1,
                failed_batches: 1,
                dropped_batches: 0,
                capacity_removed: 0,
            }
        );
        let state = store.state.lock().unwrap();
        assert_eq!(state.batches, 1);
        assert!(state.shutdown);
    }
}
