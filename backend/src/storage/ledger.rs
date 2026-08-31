use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::time::SystemTime;

use crate::ports::storage::StatsBatch;

use super::StatsSnapshot;

#[derive(Clone, Debug)]
pub struct PendingStatsBatch {
    batch: StatsBatch,
    attempts: u32,
}

impl PendingStatsBatch {
    pub fn batch(&self) -> &StatsBatch {
        &self.batch
    }

    pub const fn attempts(&self) -> u32 {
        self.attempts
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchReceipt {
    pub batch_id: u64,
    pub max_event_sequence: u64,
    pub counter_epoch: u64,
    pub committed_at: SystemTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchDecision {
    New,
    RetryPending { attempts: u32 },
    AlreadyCommitted,
    PayloadConflict,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum BatchLedgerError {
    #[error("cannot enqueue an empty stats snapshot")]
    EmptySnapshot,
    #[error("batch id space exhausted")]
    BatchIdExhausted,
    #[error("unknown pending batch")]
    UnknownBatch,
    #[error("batch payload conflicts with its existing ledger entry")]
    PayloadConflict,
}

#[derive(Clone, Copy, Debug)]
struct LedgerEntry {
    payload_fingerprint: u64,
    receipt: BatchReceipt,
}

/// 内存 batch ledger：持久化 writer 可据此实现 at-least-once + 幂等重试。
pub struct BatchLedger {
    next_batch_id: u64,
    pending: BTreeMap<u64, PendingStatsBatch>,
    committed: BTreeMap<u64, LedgerEntry>,
}

impl Default for BatchLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchLedger {
    pub const fn new() -> Self {
        Self {
            next_batch_id: 1,
            pending: BTreeMap::new(),
            committed: BTreeMap::new(),
        }
    }

    pub const fn next_batch_id(&self) -> u64 {
        self.next_batch_id
    }

    pub fn enqueue(&mut self, snapshot: StatsSnapshot) -> Result<u64, BatchLedgerError> {
        if snapshot.event_count() == 0 {
            return Err(BatchLedgerError::EmptySnapshot);
        }
        let batch_id = self.next_batch_id;
        self.next_batch_id = batch_id
            .checked_add(1)
            .ok_or(BatchLedgerError::BatchIdExhausted)?;
        let batch = snapshot.into_batch(batch_id);
        self.pending
            .insert(batch_id, PendingStatsBatch { batch, attempts: 0 });
        Ok(batch_id)
    }

    pub fn decision(&self, batch: &StatsBatch) -> BatchDecision {
        let payload_fingerprint = fingerprint(batch);
        if let Some(entry) = self.committed.get(&batch.batch_id) {
            return if entry.payload_fingerprint == payload_fingerprint {
                BatchDecision::AlreadyCommitted
            } else {
                BatchDecision::PayloadConflict
            };
        }
        if let Some(pending) = self.pending.get(&batch.batch_id) {
            return if fingerprint(&pending.batch) == payload_fingerprint {
                BatchDecision::RetryPending {
                    attempts: pending.attempts,
                }
            } else {
                BatchDecision::PayloadConflict
            };
        }
        BatchDecision::New
    }

    pub fn retry(&self, batch_id: u64) -> Option<&PendingStatsBatch> {
        self.pending.get(&batch_id)
    }

    pub fn mark_failed(&mut self, batch_id: u64) -> Result<(), BatchLedgerError> {
        let pending = self
            .pending
            .get_mut(&batch_id)
            .ok_or(BatchLedgerError::UnknownBatch)?;
        pending.attempts = pending.attempts.saturating_add(1);
        Ok(())
    }

    pub fn commit(
        &mut self,
        batch_id: u64,
        committed_at: SystemTime,
    ) -> Result<BatchDecision, BatchLedgerError> {
        if self.committed.contains_key(&batch_id) {
            return Ok(BatchDecision::AlreadyCommitted);
        }
        let pending = self
            .pending
            .remove(&batch_id)
            .ok_or(BatchLedgerError::UnknownBatch)?;
        let receipt = BatchReceipt {
            batch_id,
            max_event_sequence: pending.batch.max_event_sequence,
            counter_epoch: pending.batch.counter_epoch,
            committed_at,
        };
        self.committed.insert(
            batch_id,
            LedgerEntry {
                payload_fingerprint: fingerprint(&pending.batch),
                receipt,
            },
        );
        Ok(BatchDecision::New)
    }

    pub fn receipt(&self, batch_id: u64) -> Option<BatchReceipt> {
        self.committed.get(&batch_id).map(|entry| entry.receipt)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn pending_event_count(&self) -> u64 {
        self.pending
            .values()
            .map(|batch| batch.batch.events.len() as u64)
            .sum()
    }

    pub fn committed_count(&self) -> usize {
        self.committed.len()
    }
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
    use crate::dns::TransportClass;
    use crate::ports::storage::StatsDimension;

    use super::*;
    use crate::storage::{PersistenceGapState, StatsAccumulator};

    #[test]
    fn batch_ids_are_monotonic_and_duplicate_commit_is_idempotent() {
        let accumulator = StatsAccumulator::new(1).unwrap();
        accumulator
            .record(
                20_260_831,
                vec![StatsDimension::transport(TransportClass::Datagram)],
            )
            .unwrap();
        let first = accumulator.swap_epoch();
        accumulator
            .record(
                20_260_832,
                vec![StatsDimension::transport(TransportClass::Stream)],
            )
            .unwrap();
        let second = accumulator.swap_epoch();

        let mut ledger = BatchLedger::new();
        let first_id = ledger.enqueue(first).unwrap();
        let second_id = ledger.enqueue(second).unwrap();
        assert_eq!((first_id, second_id), (1, 2));
        let first_batch = ledger.retry(first_id).unwrap().batch().clone();
        assert!(matches!(
            ledger.decision(&first_batch),
            BatchDecision::RetryPending { attempts: 0 }
        ));
        ledger.mark_failed(first_id).unwrap();
        assert!(matches!(
            ledger.decision(&ledger.retry(first_id).unwrap().batch),
            BatchDecision::RetryPending { attempts: 1 }
        ));
        assert_eq!(
            ledger.commit(first_id, SystemTime::UNIX_EPOCH).unwrap(),
            BatchDecision::New
        );
        assert_eq!(
            ledger.decision(&first_batch),
            BatchDecision::AlreadyCommitted
        );
        assert_eq!(
            ledger.commit(first_id, SystemTime::UNIX_EPOCH).unwrap(),
            BatchDecision::AlreadyCommitted
        );
        assert!(matches!(
            ledger.decision(&StatsBatch {
                batch_id: first_id,
                max_event_sequence: 1,
                counter_epoch: 0,
                events: vec![],
            }),
            BatchDecision::PayloadConflict
        ));
        assert_eq!(ledger.committed_count(), 1);
    }

    #[test]
    fn persistence_gap_distinguishes_active_and_pending_data() {
        let accumulator = StatsAccumulator::new(1).unwrap();
        let mut ledger = BatchLedger::new();
        accumulator.record(20_260_831, vec![]).unwrap();
        assert!(matches!(
            accumulator.persistence_gap(&ledger),
            PersistenceGapState::ActiveEpoch {
                epoch: 0,
                event_count: 1
            }
        ));
        let batch_id = ledger.enqueue(accumulator.swap_epoch()).unwrap();
        accumulator.record(20_260_832, vec![]).unwrap();
        assert!(matches!(
            accumulator.persistence_gap(&ledger),
            PersistenceGapState::ActiveAndPending {
                epoch: 1,
                active_event_count: 1,
                batch_count: 1,
                pending_event_count: 1
            }
        ));
        ledger.mark_failed(batch_id).unwrap();
        ledger.commit(batch_id, SystemTime::UNIX_EPOCH).unwrap();
        assert!(matches!(
            accumulator.persistence_gap(&ledger),
            PersistenceGapState::ActiveEpoch {
                epoch: 1,
                event_count: 1
            }
        ));
    }
}
