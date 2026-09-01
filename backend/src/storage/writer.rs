//! 无外部依赖的统计 writer 边界。
//!
//! 该 adapter 用内存状态模拟业务 SQLite 的事务和 batch ledger，供 Runtime/Storage
//! 接线及 contract tests 使用。真实 SQLite pool、PRAGMA 和 migration 执行留给后续阶段。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use crate::dns::Deadline;
use crate::ports::storage::{
    SchemaVersion, StatsBatch, StorageBackend, StorageFlushSummary, StorageHealth,
    StorageOperation, StorageTransaction,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::BatchReceipt;

pub const STORAGE_SCHEMA_VERSION: SchemaVersion = SchemaVersion(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommittedBatch {
    fingerprint: u64,
    max_event_sequence: u64,
    counter_epoch: u64,
    committed_at: SystemTime,
}

#[derive(Clone)]
struct State {
    schema_version: SchemaVersion,
    health: StorageHealth,
    daily_totals: BTreeMap<i32, u64>,
    daily_dimensions: HashMap<(i32, crate::ports::storage::StatsDimension), u64>,
    committed_batches: BTreeMap<u64, CommittedBatch>,
    detail_records: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SchemaVersion(0),
            health: StorageHealth::Healthy,
            daily_totals: BTreeMap::new(),
            daily_dimensions: HashMap::new(),
            committed_batches: BTreeMap::new(),
            detail_records: 0,
        }
    }
}

/// Stats SQLite writer 的可替换内存实现。
///
/// 每个 `execute` 都在状态副本上应用，全部 operation 成功后才发布副本，因此可以
/// 在不引入 SQLite 依赖的情况下验证单事务 upsert、幂等提交和冲突回滚语义。
#[derive(Clone, Default)]
pub struct InMemoryStorageBackend {
    state: Arc<Mutex<State>>,
}

impl InMemoryStorageBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn schema_version(&self) -> SchemaVersion {
        self.state
            .lock()
            .expect("storage state lock poisoned")
            .schema_version
    }

    pub fn health(&self) -> StorageHealth {
        self.state
            .lock()
            .expect("storage state lock poisoned")
            .health
    }

    pub fn total_for_day(&self, day_utc: i32) -> u64 {
        self.state
            .lock()
            .expect("storage state lock poisoned")
            .daily_totals
            .get(&day_utc)
            .copied()
            .unwrap_or(0)
    }

    pub fn dimension_count(
        &self,
        day_utc: i32,
        dimension: &crate::ports::storage::StatsDimension,
    ) -> u64 {
        self.state
            .lock()
            .expect("storage state lock poisoned")
            .daily_dimensions
            .get(&(day_utc, dimension.clone()))
            .copied()
            .unwrap_or(0)
    }

    pub fn committed_batch_count(&self) -> usize {
        self.state
            .lock()
            .expect("storage state lock poisoned")
            .committed_batches
            .len()
    }

    pub fn batch_receipt(&self, batch_id: u64) -> Option<BatchReceipt> {
        self.state
            .lock()
            .expect("storage state lock poisoned")
            .committed_batches
            .get(&batch_id)
            .map(|batch| BatchReceipt {
                batch_id,
                max_event_sequence: batch.max_event_sequence,
                counter_epoch: batch.counter_epoch,
                committed_at: batch.committed_at,
            })
    }

    pub fn detail_record_count(&self) -> u64 {
        self.state
            .lock()
            .expect("storage state lock poisoned")
            .detail_records
    }

    fn check_deadline(deadline: Deadline, operation: &'static str) -> Result<(), PortError> {
        if deadline.is_expired(Instant::now()) {
            Err(PortError::new(PortErrorClass::Timeout, operation))
        } else {
            Ok(())
        }
    }

    fn ensure_ready(state: &State, operation: &'static str) -> Result<(), PortError> {
        match state.health {
            StorageHealth::Healthy => {
                if state.schema_version == STORAGE_SCHEMA_VERSION {
                    Ok(())
                } else {
                    Err(PortError::new(PortErrorClass::Unavailable, operation)
                        .with_safe_context("schema not migrated"))
                }
            }
            StorageHealth::Stopping => Err(PortError::new(PortErrorClass::Unavailable, operation)
                .with_safe_context("stopping")),
            StorageHealth::Degraded | StorageHealth::Failed => {
                Err(PortError::new(PortErrorClass::Unavailable, operation))
            }
        }
    }

    fn migrate_now(
        &self,
        target: SchemaVersion,
        deadline: Deadline,
    ) -> Result<SchemaVersion, PortError> {
        Self::check_deadline(deadline, "storage migration")?;
        let mut state = self.state.lock().expect("storage state lock poisoned");
        if state.health == StorageHealth::Stopping {
            return Err(
                PortError::new(PortErrorClass::Unavailable, "storage migration")
                    .with_safe_context("stopping"),
            );
        }
        if target != STORAGE_SCHEMA_VERSION {
            return Err(
                PortError::new(PortErrorClass::InvalidInput, "storage migration")
                    .with_safe_context("unsupported schema version"),
            );
        }
        state.schema_version = target;
        Ok(target)
    }

    fn execute_now(
        &self,
        transaction: StorageTransaction,
        deadline: Deadline,
    ) -> Result<(), PortError> {
        Self::check_deadline(deadline, "storage execute")?;
        if transaction.idempotency_key.is_empty() || transaction.operations.is_empty() {
            return Err(
                PortError::new(PortErrorClass::InvalidInput, "storage execute")
                    .with_safe_context("empty transaction"),
            );
        }

        let mut state = self.state.lock().expect("storage state lock poisoned");
        Self::ensure_ready(&state, "storage execute")?;
        let mut candidate = state.clone();
        for operation in transaction.operations {
            match operation {
                StorageOperation::StatsBatch(batch) => apply_stats_batch(&mut candidate, &batch)?,
                StorageOperation::ResolveBatch(_) => {
                    return Err(PortError::new(
                        PortErrorClass::Unavailable,
                        "resolve detail writer",
                    )
                    .with_safe_context("deferred"));
                }
            }
        }
        *state = candidate;
        Ok(())
    }
}

impl StorageBackend for InMemoryStorageBackend {
    fn migrate(
        &self,
        target: SchemaVersion,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<SchemaVersion, PortError>> {
        Box::pin(async move { self.migrate_now(target, deadline) })
    }

    fn execute(
        &self,
        transaction: StorageTransaction,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.execute_now(transaction, deadline) })
    }

    fn health_probe(&self, deadline: Deadline) -> PortFuture<'_, Result<StorageHealth, PortError>> {
        Box::pin(async move {
            Self::check_deadline(deadline, "storage health probe")?;
            Ok(self.health())
        })
    }

    fn checkpoint(&self, deadline: Deadline) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            Self::check_deadline(deadline, "storage checkpoint")?;
            let state = self.state.lock().expect("storage state lock poisoned");
            Self::ensure_ready(&state, "storage checkpoint")
        })
    }

    fn flush(&self, deadline: Deadline) -> PortFuture<'_, Result<StorageFlushSummary, PortError>> {
        Box::pin(async move {
            Self::check_deadline(deadline, "storage flush")?;
            let state = self.state.lock().expect("storage state lock poisoned");
            Self::ensure_ready(&state, "storage flush")?;
            Ok(StorageFlushSummary::default())
        })
    }

    fn shutdown(
        &self,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<StorageFlushSummary, PortError>> {
        Box::pin(async move {
            Self::check_deadline(deadline, "storage shutdown")?;
            let mut state = self.state.lock().expect("storage state lock poisoned");
            if state.health == StorageHealth::Healthy {
                state.health = StorageHealth::Stopping;
            }
            Ok(StorageFlushSummary::default())
        })
    }
}

fn apply_stats_batch(state: &mut State, batch: &StatsBatch) -> Result<(), PortError> {
    if batch.batch_id == 0 || batch.events.is_empty() {
        return Err(PortError::new(PortErrorClass::InvalidInput, "stats batch")
            .with_safe_context("empty or invalid batch"));
    }
    let fingerprint = fingerprint(batch);
    if let Some(committed) = state.committed_batches.get(&batch.batch_id) {
        if committed.fingerprint == fingerprint {
            return Ok(());
        }
        return Err(PortError::new(PortErrorClass::CorruptData, "stats batch")
            .with_safe_context("batch payload conflict"));
    }

    let mut sequences = BTreeSet::new();
    let max_sequence = batch
        .events
        .iter()
        .map(crate::ports::storage::StatsEvent::sequence)
        .max()
        .unwrap_or(0);
    if max_sequence != batch.max_event_sequence
        || !batch
            .events
            .iter()
            .all(|event| sequences.insert(event.sequence()))
    {
        return Err(PortError::new(PortErrorClass::InvalidInput, "stats batch")
            .with_safe_context("invalid event sequence"));
    }

    for event in &batch.events {
        increment_counter(state.daily_totals.entry(event.day_utc()).or_default(), 1)?;
        for dimension in event.dimensions() {
            increment_counter(
                state
                    .daily_dimensions
                    .entry((event.day_utc(), dimension.clone()))
                    .or_default(),
                1,
            )?;
        }
    }
    state.committed_batches.insert(
        batch.batch_id,
        CommittedBatch {
            fingerprint,
            max_event_sequence: batch.max_event_sequence,
            counter_epoch: batch.counter_epoch,
            committed_at: SystemTime::now(),
        },
    );
    Ok(())
}

fn increment_counter(counter: &mut u64, amount: u64) -> Result<(), PortError> {
    *counter = counter.checked_add(amount).ok_or_else(|| {
        PortError::new(PortErrorClass::ResourceExhausted, "stats counter")
            .with_safe_context("counter overflow")
    })?;
    Ok(())
}

fn fingerprint(batch: &StatsBatch) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    batch.batch_id.hash(&mut hasher);
    batch.max_event_sequence.hash(&mut hasher);
    batch.counter_epoch.hash(&mut hasher);
    for event in &batch.events {
        event.sequence().hash(&mut hasher);
        event.day_utc().hash(&mut hasher);
        event.dimensions().hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::dns::TransportClass;
    use crate::ports::storage::{
        StatsDimension, StorageBackend, StorageOperation, StorageTransaction,
    };

    use super::*;

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(30))
    }

    fn transaction(batch: StatsBatch) -> StorageTransaction {
        StorageTransaction {
            idempotency_key: Arc::from(format!("batch-{}", batch.batch_id)),
            operations: vec![StorageOperation::StatsBatch(batch)],
        }
    }

    #[test]
    fn migration_declares_business_tables_and_dimension_allowlist() {
        let sql = include_str!("../../migrations/0001_storage.sql");
        for table in [
            "storage_meta",
            "stats_daily_total",
            "stats_daily_dimension",
            "stats_batch_ledger",
            "resolve_log",
        ] {
            assert!(sql.contains(&format!("CREATE TABLE {table}")));
        }
        assert!(sql.contains("'client_bucket'"));
        assert!(sql.contains("'attempt_outcome'"));
        assert!(sql.contains("PRIMARY KEY (day_utc, dimension_kind, dimension_value)"));
    }

    #[tokio::test]
    async fn migrates_and_commits_stats_batch_atomically() {
        let backend = InMemoryStorageBackend::new();
        assert_eq!(backend.schema_version(), SchemaVersion(0));
        assert_eq!(
            backend
                .migrate(STORAGE_SCHEMA_VERSION, deadline())
                .await
                .unwrap(),
            STORAGE_SCHEMA_VERSION
        );
        let event = crate::ports::storage::StatsEvent::new(
            7,
            20_260_902,
            vec![StatsDimension::transport(TransportClass::Datagram)],
        )
        .unwrap();
        backend
            .execute(
                transaction(StatsBatch {
                    batch_id: 1,
                    max_event_sequence: 7,
                    counter_epoch: 2,
                    events: vec![event.clone()],
                }),
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(backend.total_for_day(20_260_902), 1);
        assert_eq!(
            backend.dimension_count(
                20_260_902,
                &StatsDimension::transport(TransportClass::Datagram)
            ),
            1
        );
        assert_eq!(backend.committed_batch_count(), 1);
        assert_eq!(backend.batch_receipt(1).unwrap().counter_epoch, 2);
    }

    #[tokio::test]
    async fn retries_are_idempotent_and_conflicts_roll_back_transaction() {
        let backend = InMemoryStorageBackend::new();
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let batch = StatsBatch {
            batch_id: 2,
            max_event_sequence: 8,
            counter_epoch: 3,
            events: vec![crate::ports::storage::StatsEvent::new(8, 20_260_902, vec![]).unwrap()],
        };
        backend
            .execute(transaction(batch.clone()), deadline())
            .await
            .unwrap();
        backend
            .execute(transaction(batch.clone()), deadline())
            .await
            .unwrap();
        assert_eq!(backend.total_for_day(20_260_902), 1);

        let conflicting = StatsBatch {
            batch_id: 2,
            max_event_sequence: 9,
            counter_epoch: 3,
            events: vec![crate::ports::storage::StatsEvent::new(9, 20_260_903, vec![]).unwrap()],
        };
        let error = backend
            .execute(transaction(conflicting), deadline())
            .await
            .unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::CorruptData
        ));
        assert_eq!(backend.total_for_day(20_260_902), 1);
        assert_eq!(backend.total_for_day(20_260_903), 0);
    }

    #[tokio::test]
    async fn detail_operations_are_explicitly_deferred_and_do_not_partially_commit() {
        let backend = InMemoryStorageBackend::new();
        backend
            .migrate(STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let stats = StatsBatch {
            batch_id: 3,
            max_event_sequence: 10,
            counter_epoch: 4,
            events: vec![crate::ports::storage::StatsEvent::new(10, 20_260_902, vec![]).unwrap()],
        };
        let transaction = StorageTransaction {
            idempotency_key: Arc::from("stats-and-details"),
            operations: vec![
                StorageOperation::StatsBatch(stats),
                StorageOperation::ResolveBatch(Vec::new()),
            ],
        };
        let error = backend.execute(transaction, deadline()).await.unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Unavailable
        ));
        assert_eq!(backend.total_for_day(20_260_902), 0);
        assert_eq!(backend.detail_record_count(), 0);
    }
}
