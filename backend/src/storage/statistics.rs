use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ports::storage::{
    MAX_STATS_DIMENSIONS, StatsBatch, StatsDimension, StatsEvent, StatsEventError,
};

use super::BatchLedger;

const DEFAULT_SHARD_COUNT: usize = 16;
const SECONDS_PER_DAY: u64 = 86_400;

/// 将时间转换为 Unix epoch 起算的 UTC 自然日编号。
pub fn day_utc(occurred_at: SystemTime) -> Result<i32, StatsAccumulatorError> {
    let day = match occurred_at.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() / SECONDS_PER_DAY) as i64,
        Err(error) => -(error.duration().as_secs().div_ceil(SECONDS_PER_DAY) as i64),
    };
    i32::try_from(day).map_err(|_| StatsAccumulatorError::DayOutOfRange)
}

#[derive(Debug, thiserror::Error)]
pub enum StatsAccumulatorError {
    #[error("invalid stats event: {0}")]
    InvalidEvent(#[from] StatsEventError),
    #[error("stats accumulator requires at least one shard")]
    InvalidShardCount,
    #[error("stats counter overflow")]
    CounterOverflow,
    #[error("stats event sequence space exhausted")]
    SequenceExhausted,
    #[error("UTC day is outside the supported range")]
    DayOutOfRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DimensionCount {
    pub day_utc: i32,
    pub dimension: StatsDimension,
    pub count: u64,
}

#[derive(Clone, Debug)]
pub struct StatsSnapshot {
    counter_epoch: u64,
    max_event_sequence: u64,
    event_count: u64,
    totals: Vec<(i32, u64)>,
    dimensions: Vec<DimensionCount>,
    events: Vec<StatsEvent>,
}

impl StatsSnapshot {
    pub const fn counter_epoch(&self) -> u64 {
        self.counter_epoch
    }

    pub const fn max_event_sequence(&self) -> u64 {
        self.max_event_sequence
    }

    pub const fn event_count(&self) -> u64 {
        self.event_count
    }

    pub fn total_for_day(&self, day_utc: i32) -> u64 {
        self.totals
            .iter()
            .find_map(|(day, count)| (*day == day_utc).then_some(*count))
            .unwrap_or(0)
    }

    pub fn totals(&self) -> &[(i32, u64)] {
        &self.totals
    }

    pub fn dimensions(&self) -> &[DimensionCount] {
        &self.dimensions
    }

    pub fn events(&self) -> &[StatsEvent] {
        &self.events
    }

    pub(crate) fn into_batch(self, batch_id: u64) -> StatsBatch {
        StatsBatch {
            batch_id,
            max_event_sequence: self.max_event_sequence,
            counter_epoch: self.counter_epoch,
            events: self.events,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceGapState {
    Clear,
    ActiveEpoch {
        epoch: u64,
        event_count: u64,
    },
    PendingBatches {
        batch_count: usize,
        event_count: u64,
    },
    ActiveAndPending {
        epoch: u64,
        active_event_count: u64,
        batch_count: usize,
        pending_event_count: u64,
    },
}

struct Shard {
    totals: HashMap<i32, u64>,
    dimensions: HashMap<(i32, StatsDimension), u64>,
    events: Vec<StatsEvent>,
    max_event_sequence: u64,
}

impl Shard {
    fn new() -> Self {
        Self {
            totals: HashMap::new(),
            dimensions: HashMap::new(),
            events: Vec::new(),
            max_event_sequence: 0,
        }
    }
}

struct EpochCounters {
    epoch: u64,
    shards: Vec<Mutex<Shard>>,
}

impl EpochCounters {
    fn new(epoch: u64, shard_count: usize) -> Self {
        Self {
            epoch,
            shards: (0..shard_count).map(|_| Mutex::new(Shard::new())).collect(),
        }
    }

    fn shard_index(&self, day_utc: i32, dimensions: &[StatsDimension]) -> usize {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        day_utc.hash(&mut hasher);
        dimensions.hash(&mut hasher);
        (hasher.finish() as usize) % self.shards.len()
    }
}

/// 无 await 的分片统计累加器。
pub struct StatsAccumulator {
    next_event_sequence: AtomicU64,
    active: RwLock<Arc<EpochCounters>>,
    shard_count: usize,
}

impl StatsAccumulator {
    pub fn new(shard_count: usize) -> Result<Self, StatsAccumulatorError> {
        if shard_count == 0 {
            return Err(StatsAccumulatorError::InvalidShardCount);
        }
        Ok(Self {
            next_event_sequence: AtomicU64::new(1),
            active: RwLock::new(Arc::new(EpochCounters::new(0, shard_count))),
            shard_count,
        })
    }

    pub fn with_default_shards() -> Self {
        Self::new(DEFAULT_SHARD_COUNT).expect("default shard count is non-zero")
    }

    pub const fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// 记录一个请求；sequence 由 accumulator 单调分配，total 只增加一次。
    pub fn record(
        &self,
        day_utc: i32,
        dimensions: Vec<StatsDimension>,
    ) -> Result<u64, StatsAccumulatorError> {
        let sequence = self
            .next_event_sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| StatsAccumulatorError::SequenceExhausted)?;
        let event = StatsEvent::new(sequence, day_utc, dimensions)?;
        self.record_event(event)?;
        Ok(sequence)
    }

    fn record_event(&self, event: StatsEvent) -> Result<(), StatsAccumulatorError> {
        if event.dimensions().len() > MAX_STATS_DIMENSIONS {
            return Err(StatsAccumulatorError::InvalidEvent(
                StatsEventError::TooManyDimensions,
            ));
        }

        let active = self.active.read().expect("stats active lock poisoned");
        let index = active.shard_index(event.day_utc(), event.dimensions());
        let mut shard = active.shards[index]
            .lock()
            .expect("stats shard lock poisoned");

        let total = shard.totals.get(&event.day_utc()).copied().unwrap_or(0);
        total
            .checked_add(1)
            .ok_or(StatsAccumulatorError::CounterOverflow)?;
        for dimension in event.dimensions() {
            let key = (event.day_utc(), dimension.clone());
            let count = shard.dimensions.get(&key).copied().unwrap_or(0);
            count
                .checked_add(1)
                .ok_or(StatsAccumulatorError::CounterOverflow)?;
        }

        *shard.totals.entry(event.day_utc()).or_default() += 1;
        for dimension in event.dimensions() {
            *shard
                .dimensions
                .entry((event.day_utc(), dimension.clone()))
                .or_default() += 1;
        }
        shard.max_event_sequence = shard.max_event_sequence.max(event.sequence());
        shard.events.push(event);
        Ok(())
    }

    /// 原子切换 epoch；切换后新请求只进入新 epoch，返回的 snapshot 可异步持久化。
    pub fn swap_epoch(&self) -> StatsSnapshot {
        let mut active = self.active.write().expect("stats active lock poisoned");
        let next_epoch = active.epoch.checked_add(1).expect("stats epoch exhausted");
        let previous = std::mem::replace(
            &mut *active,
            Arc::new(EpochCounters::new(next_epoch, self.shard_count)),
        );
        snapshot_epoch(&previous)
    }

    pub fn persistence_gap(&self, ledger: &BatchLedger) -> PersistenceGapState {
        let active = self.active.read().expect("stats active lock poisoned");
        let active_event_count = epoch_event_count(&active);
        let pending_count = ledger.pending_count();
        let pending_events = ledger.pending_event_count();
        match (active_event_count, pending_count) {
            (0, 0) => PersistenceGapState::Clear,
            (active_count, 0) => PersistenceGapState::ActiveEpoch {
                epoch: active.epoch,
                event_count: active_count,
            },
            (0, count) => PersistenceGapState::PendingBatches {
                batch_count: count,
                event_count: pending_events,
            },
            (active_count, count) => PersistenceGapState::ActiveAndPending {
                epoch: active.epoch,
                active_event_count: active_count,
                batch_count: count,
                pending_event_count: pending_events,
            },
        }
    }
}

fn snapshot_epoch(epoch: &EpochCounters) -> StatsSnapshot {
    let mut totals = HashMap::new();
    let mut dimensions = HashMap::new();
    let mut events = Vec::new();
    let mut max_event_sequence = 0;

    for shard in &epoch.shards {
        let shard = shard.lock().expect("stats shard lock poisoned");
        for (day, count) in &shard.totals {
            *totals.entry(*day).or_insert(0) += count;
        }
        for (key, count) in &shard.dimensions {
            *dimensions.entry(key.clone()).or_insert(0) += count;
        }
        max_event_sequence = max_event_sequence.max(shard.max_event_sequence);
        events.extend(shard.events.iter().cloned());
    }

    let mut totals = totals.into_iter().collect::<Vec<_>>();
    totals.sort_unstable_by_key(|(day, _)| *day);
    let mut dimensions = dimensions
        .into_iter()
        .map(|((day_utc, dimension), count)| DimensionCount {
            day_utc,
            dimension,
            count,
        })
        .collect::<Vec<_>>();
    dimensions.sort_unstable_by_key(|entry| entry.day_utc);
    events.sort_unstable_by_key(StatsEvent::sequence);

    StatsSnapshot {
        counter_epoch: epoch.epoch,
        max_event_sequence,
        event_count: events.len() as u64,
        totals,
        dimensions,
        events,
    }
}

fn epoch_event_count(epoch: &EpochCounters) -> u64 {
    epoch
        .shards
        .iter()
        .map(|shard| {
            shard
                .lock()
                .expect("stats shard lock poisoned")
                .events
                .len() as u64
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::dns::TransportClass;
    use crate::ports::storage::StatsSource;

    use super::*;

    #[test]
    fn day_utc_handles_epoch_and_cross_midnight() {
        assert_eq!(day_utc(SystemTime::UNIX_EPOCH).unwrap(), 0);
        assert_eq!(
            day_utc(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400)).unwrap(),
            1
        );
        assert_eq!(
            day_utc(SystemTime::UNIX_EPOCH - Duration::from_secs(1)).unwrap(),
            -1
        );
    }

    #[test]
    fn accumulator_deduplicates_dimension_kind_and_swaps_epoch() {
        let accumulator = StatsAccumulator::new(2).unwrap();
        accumulator
            .record(
                20_260_831,
                vec![
                    StatsDimension::transport(TransportClass::Datagram),
                    StatsDimension::source(StatsSource::Hosts),
                ],
            )
            .unwrap();
        accumulator
            .record(
                20_260_832,
                vec![StatsDimension::transport(TransportClass::Datagram)],
            )
            .unwrap();

        let snapshot = accumulator.swap_epoch();
        assert_eq!(snapshot.counter_epoch(), 0);
        assert_eq!(snapshot.event_count(), 2);
        assert_eq!(snapshot.total_for_day(20_260_831), 1);
        assert_eq!(snapshot.total_for_day(20_260_832), 1);
        assert_eq!(snapshot.dimensions().len(), 3);

        accumulator
            .record(20_260_832, vec![StatsDimension::source(StatsSource::Cache)])
            .unwrap();
        assert_eq!(accumulator.swap_epoch().counter_epoch(), 1);
    }

    #[test]
    fn invalid_or_duplicate_dimensions_are_rejected_before_mutation() {
        let accumulator = StatsAccumulator::new(1).unwrap();
        let transport = StatsDimension::transport(TransportClass::Datagram);
        let error = accumulator
            .record(20_260_831, vec![transport.clone(), transport])
            .unwrap_err();
        assert!(matches!(error, StatsAccumulatorError::InvalidEvent(_)));
        assert_eq!(accumulator.swap_epoch().event_count(), 0);
    }
}
