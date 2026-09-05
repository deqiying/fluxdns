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

/// Cache persistence worker 在本次生命周期内产生的安全计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachePersistenceRunSummary {
    /// 已成功提交到底层持久化 adapter 的批次数。
    pub persisted_batches: u64,
    /// adapter 写入失败的批次数。
    pub failed_batches: u64,
    /// 有界队列满或关闭时丢弃的批次数。
    pub dropped_batches: u64,
    /// 停机容量维护阶段删除的过期批次数。
    pub capacity_removed: u64,
}

impl CachePersistenceRunSummary {
    /// 合并另一个 persistence owner 的安全计数，避免热重载后遗漏旧 Runtime 摘要。
    pub fn merge(&mut self, other: Self) {
        self.persisted_batches = self
            .persisted_batches
            .saturating_add(other.persisted_batches);
        self.failed_batches = self.failed_batches.saturating_add(other.failed_batches);
        self.dropped_batches = self.dropped_batches.saturating_add(other.dropped_batches);
        self.capacity_removed = self.capacity_removed.saturating_add(other.capacity_removed);
    }

    /// 返回运行期间是否出现未持久化批次。
    pub const fn has_persistence_gap(self) -> bool {
        self.failed_batches > 0 || self.dropped_batches > 0
    }
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
        panic_next: bool,
        block_shutdown: bool,
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
                let panic_next = std::mem::take(&mut self.state.lock().unwrap().panic_next);
                if panic_next {
                    panic!("synthetic private adapter panic payload");
                }
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
                let block_shutdown = self.state.lock().unwrap().block_shutdown;
                if block_shutdown {
                    std::future::pending::<()>().await;
                }
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

    /// V1-O02：内部持久化 worker panic 由 owner 回收，返回稳定分类而非原始 payload。
    #[tokio::test]
    async fn contract_v1_persistence_worker_panic_is_reclaimed_and_sanitized() {
        let store = Arc::new(FakePersistentStore::default());
        store.state.lock().unwrap().panic_next = true;
        let runtime =
            CachePersistenceRuntime::start(store.clone(), 2, Duration::from_secs(1)).unwrap();
        let writer = runtime.writer();
        assert!(writer.enqueue(empty_batch()).is_ok());
        tokio::time::timeout(Duration::from_secs(5), writer.sender.closed())
            .await
            .unwrap();
        let error = runtime
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Internal
        ));
        assert_eq!(error.operation(), "cache_persistence.worker");
        assert!(!format!("{error:?}").contains("private adapter"));
        assert!(runtime.task.lock().unwrap().is_none());
        assert!(writer.enqueue(empty_batch()).is_err());
        // 不把 panic 当作自动恢复；真实 store 的关闭由测试明确补做。
        assert!(!store.state.lock().unwrap().shutdown);
        store
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .await
            .unwrap();
    }

    #[test]
    fn run_summary_merges_owners_and_detects_persistence_gaps() {
        let mut summary = CachePersistenceRunSummary {
            persisted_batches: u64::MAX,
            failed_batches: 0,
            dropped_batches: 1,
            capacity_removed: 2,
        };

        summary.merge(CachePersistenceRunSummary {
            persisted_batches: 1,
            failed_batches: 3,
            dropped_batches: 4,
            capacity_removed: 5,
        });

        assert_eq!(summary.persisted_batches, u64::MAX);
        assert_eq!(summary.failed_batches, 3);
        assert_eq!(summary.dropped_batches, 5);
        assert_eq!(summary.capacity_removed, 7);
        assert!(summary.has_persistence_gap());
        assert!(!CachePersistenceRunSummary::default().has_persistence_gap());
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

    /// 验证 persistence adapter 停机阻塞超过 deadline 时 worker 被 abort 并返回 Timeout。
    #[tokio::test]
    async fn shutdown_aborts_worker_when_store_exceeds_deadline() {
        let store = Arc::new(FakePersistentStore::default());
        store.state.lock().unwrap().block_shutdown = true;
        let runtime =
            CachePersistenceRuntime::start(store.clone(), 1, Duration::from_secs(1)).unwrap();

        let error = runtime
            .shutdown(Deadline::new(Instant::now() + Duration::from_millis(20)))
            .await
            .unwrap_err();

        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        assert_eq!(error.operation(), "cache_persistence.shutdown");
        assert!(!store.state.lock().unwrap().shutdown);
    }
}
