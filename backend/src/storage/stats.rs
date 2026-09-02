//! 聚合统计的 epoch checkpoint 与持久化 worker。

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use thiserror::Error;

use crate::dns::Deadline;
use crate::ports::storage::{
    StatsDimension, StatsEvent, StatsRecorder, StorageBackend, StorageOperation, StorageTransaction,
};
use crate::ports::{PortError, PortErrorClass};

use super::{
    BatchLedger, BatchLedgerError, PersistenceGapState, StatsAccumulator, StatsAccumulatorError,
};

/// 统计 worker 一次 flush 的可观测摘要。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatsPersistenceFlushSummary {
    pub batches_committed: u64,
    pub events_committed: u64,
    pub pending_batches: usize,
    pub persistence_gap: bool,
}

#[derive(Debug, Error)]
pub enum StatsPersistenceError {
    #[error("stats accumulator failed: {0}")]
    Accumulator(#[source] StatsAccumulatorError),
    #[error("stats batch ledger failed: {0}")]
    Ledger(#[source] BatchLedgerError),
    #[error("stats storage operation failed: {0}")]
    Backend(#[source] PortError),
}

/// 将无 await 的 stats accumulator 接入可重试的 `StorageBackend`。
pub struct StatsPersistenceWorker {
    backend: Arc<dyn StorageBackend>,
    accumulator: Arc<StatsAccumulator>,
    ledger: Mutex<BatchLedger>,
}

impl StatsPersistenceWorker {
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self::with_accumulator(backend, Arc::new(StatsAccumulator::with_default_shards()))
    }

    pub fn with_accumulator(
        backend: Arc<dyn StorageBackend>,
        accumulator: Arc<StatsAccumulator>,
    ) -> Self {
        Self {
            backend,
            accumulator,
            ledger: Mutex::new(BatchLedger::new()),
        }
    }

    pub fn accumulator(&self) -> &Arc<StatsAccumulator> {
        &self.accumulator
    }

    /// 请求热路径入口；只执行内存统计，不 await 或访问数据库。
    pub fn record_request(
        &self,
        day_utc: i32,
        dimensions: Vec<StatsDimension>,
    ) -> Result<u64, StatsAccumulatorError> {
        self.accumulator.record(day_utc, dimensions)
    }

    pub fn pending_batch_count(&self) -> usize {
        self.ledger
            .lock()
            .expect("stats ledger lock poisoned")
            .pending_count()
    }

    pub fn persistence_gap(&self) -> PersistenceGapState {
        let ledger = self.ledger.lock().expect("stats ledger lock poisoned");
        self.accumulator.persistence_gap(&ledger)
    }

    /// 原子冻结当前 epoch，并按 batch ID 顺序提交所有 pending stats batch。
    pub async fn flush(
        &mut self,
        deadline: Deadline,
    ) -> Result<StatsPersistenceFlushSummary, StatsPersistenceError> {
        let snapshot = self.accumulator.swap_epoch();
        if snapshot.event_count() > 0 {
            self.ledger
                .lock()
                .expect("stats ledger lock poisoned")
                .enqueue(snapshot)
                .map_err(StatsPersistenceError::Ledger)?;
        }

        let mut summary = StatsPersistenceFlushSummary::default();
        loop {
            let batch = self
                .ledger
                .lock()
                .expect("stats ledger lock poisoned")
                .next_pending_batch();
            let Some(batch) = batch else {
                break;
            };
            let batch_id = batch.batch_id;
            let event_count = batch.events.len() as u64;
            let transaction = StorageTransaction {
                idempotency_key: Arc::from(format!("stats-batch-{batch_id}")),
                operations: vec![StorageOperation::StatsBatch(batch)],
            };
            if let Err(error) = self.backend.execute(transaction, deadline).await {
                self.ledger
                    .lock()
                    .expect("stats ledger lock poisoned")
                    .mark_failed(batch_id)
                    .map_err(StatsPersistenceError::Ledger)?;
                return Err(StatsPersistenceError::Backend(error));
            }
            self.ledger
                .lock()
                .expect("stats ledger lock poisoned")
                .commit(batch_id, SystemTime::now())
                .map_err(StatsPersistenceError::Ledger)?;
            summary.batches_committed = summary.batches_committed.saturating_add(1);
            summary.events_committed = summary.events_committed.saturating_add(event_count);
        }

        let ledger = self.ledger.lock().expect("stats ledger lock poisoned");
        summary.pending_batches = ledger.pending_count();
        summary.persistence_gap = !matches!(
            self.accumulator.persistence_gap(&ledger),
            PersistenceGapState::Clear
        );
        Ok(summary)
    }
}

impl StatsRecorder for StatsPersistenceWorker {
    fn record(&self, event: StatsEvent) -> Result<(), PortError> {
        self.accumulator
            .record_event(event)
            .map_err(stats_record_error)
    }
}

fn stats_record_error(error: StatsAccumulatorError) -> PortError {
    let class = match error {
        StatsAccumulatorError::InvalidEvent(_)
        | StatsAccumulatorError::DayOutOfRange
        | StatsAccumulatorError::InvalidShardCount => PortErrorClass::InvalidInput,
        StatsAccumulatorError::CounterOverflow | StatsAccumulatorError::SequenceExhausted => {
            PortErrorClass::ResourceExhausted
        }
    };
    PortError::new(class, "stats.recorder")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::dns::TransportClass;
    use crate::ports::storage::{StatsDimension, StatsRecorder, StorageBackend};

    use super::{StatsPersistenceError, StatsPersistenceWorker};
    use crate::dns::Deadline;
    use crate::storage::InMemoryStorageBackend;
    use crate::storage::{PersistenceGapState, STORAGE_SCHEMA_VERSION};

    fn deadline() -> Deadline {
        Deadline::new(std::time::Instant::now() + Duration::from_secs(30))
    }

    #[tokio::test]
    async fn flushes_epoch_to_backend_and_clears_pending_gap() {
        let backend = std::sync::Arc::new(InMemoryStorageBackend::new());
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let mut worker = StatsPersistenceWorker::new(backend.clone());
        worker
            .record_request(
                20_260_902,
                vec![StatsDimension::transport(TransportClass::Datagram)],
            )
            .unwrap();

        let summary = worker.flush(deadline()).await.unwrap();
        assert_eq!(summary.batches_committed, 1);
        assert_eq!(summary.events_committed, 1);
        assert_eq!(summary.pending_batches, 0);
        assert!(!summary.persistence_gap);
        assert_eq!(backend.total_for_day(20_260_902), 1);
        assert!(matches!(
            worker.persistence_gap(),
            PersistenceGapState::Clear
        ));
    }

    #[tokio::test]
    async fn recorder_preserves_event_and_advances_accumulator_sequence() {
        let backend = std::sync::Arc::new(InMemoryStorageBackend::new());
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let mut worker = StatsPersistenceWorker::new(backend.clone());
        let event = crate::ports::storage::StatsEvent::new(
            41,
            20_260_902,
            vec![StatsDimension::transport(TransportClass::Stream)],
        )
        .unwrap();
        StatsRecorder::record(&worker, event).unwrap();
        worker
            .record_request(
                20_260_902,
                vec![StatsDimension::source(
                    crate::ports::storage::StatsSource::Hosts,
                )],
            )
            .unwrap();

        worker.flush(deadline()).await.unwrap();
        assert_eq!(backend.total_for_day(20_260_902), 2);
    }

    #[tokio::test]
    async fn backend_failure_retains_pending_batch_for_retry() {
        let backend = std::sync::Arc::new(InMemoryStorageBackend::new());
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let mut worker = StatsPersistenceWorker::new(backend.clone());
        worker.record_request(20_260_902, vec![]).unwrap();
        backend.shutdown(deadline()).await.unwrap();

        assert!(matches!(
            worker.flush(deadline()).await,
            Err(StatsPersistenceError::Backend(_))
        ));
        assert_eq!(worker.pending_batch_count(), 1);
        assert!(matches!(
            worker.persistence_gap(),
            PersistenceGapState::PendingBatches { batch_count: 1, .. }
        ));
    }
}
