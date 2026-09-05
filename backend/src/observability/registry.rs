//! 正式 typed metrics 的有界聚合；不再拥有独立事件队列或 health 状态。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ports::observation::ResolutionEvent;
use crate::ports::telemetry::{
    CacheStatus, Component, MetricEvent, MetricLabelKey, MetricLabelValue, MetricName, MetricValue,
    OutcomeClass,
};

use super::lock_unpoisoned;

const MAX_METRIC_SERIES: usize = 128;
const REQUEST_SERIES: usize = 14;
const OUTCOMES: [OutcomeClass; 6] = [
    OutcomeClass::Success,
    OutcomeClass::Failure,
    OutcomeClass::Timeout,
    OutcomeClass::Cancelled,
    OutcomeClass::Rejected,
    OutcomeClass::Dropped,
];
const CACHE_STATUSES: [CacheStatus; 6] = [
    CacheStatus::Disabled,
    CacheStatus::Miss,
    CacheStatus::Fresh,
    CacheStatus::Stale,
    CacheStatus::StoreUnavailable,
    CacheStatus::WriteRejected,
];

/// 固定微秒上界，最后一桶为 +Inf；没有按请求字段创建 series 的入口。
pub const REQUEST_LATENCY_BUCKETS_MICROS: [u64; 12] = [
    1_000,
    5_000,
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    u64::MAX,
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LatencyHistogram {
    pub buckets: [u64; 12],
    pub count: u64,
    pub sum_micros: u64,
}

impl LatencyHistogram {
    fn observe(&mut self, micros: u64) -> Result<(), RegistryError> {
        self.count = self
            .count
            .checked_add(1)
            .ok_or(RegistryError::CounterOverflow)?;
        self.sum_micros = self
            .sum_micros
            .checked_add(micros)
            .ok_or(RegistryError::CounterOverflow)?;
        for (count, upper) in self.buckets.iter_mut().zip(REQUEST_LATENCY_BUCKETS_MICROS) {
            if micros <= upper {
                *count = count.checked_add(1).ok_or(RegistryError::CounterOverflow)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct RequestMetrics {
    latency: LatencyHistogram,
    core_latency: LatencyHistogram,
    outcomes: [u64; 6],
    cache: [u64; 6],
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MetricKey {
    name: MetricName,
    component: Component,
}

/// Counter 为当前 writer 实例内累计值，Gauge 为采样值；不是输入增量事件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetricSnapshot {
    pub name: MetricName,
    pub component: Component,
    pub value: MetricValue,
    pub outcome: Option<OutcomeClass>,
    pub cache_status: Option<CacheStatus>,
    pub histogram: Option<LatencyHistogram>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum RegistryError {
    #[error("metric series capacity must be greater than zero")]
    ZeroMetricCapacity,
    #[error("metric series capacity has been exhausted")]
    MetricCapacityExhausted,
    #[error("metric name or labels are not registered")]
    InvalidMetric,
    #[error("metric value does not match its registered kind")]
    MetricKindMismatch,
    #[error("metric counter overflow")]
    CounterOverflow,
}

enum MetricCell {
    Counter(AtomicU64),
    Gauge(AtomicI64),
}

impl MetricCell {
    fn value(&self) -> MetricValue {
        match self {
            Self::Counter(value) => MetricValue::Counter(value.load(Ordering::Acquire)),
            Self::Gauge(value) => MetricValue::Gauge(value.load(Ordering::Acquire)),
        }
    }
}

/// 复用有界 series、原子更新与 checked add；只开放已经接线的指标描述符。
pub(super) struct ObservabilityRegistry {
    max_metric_series: usize,
    metrics: Mutex<BTreeMap<MetricKey, Arc<MetricCell>>>,
    requests: Mutex<Option<RequestMetrics>>,
}

impl ObservabilityRegistry {
    pub(super) fn new() -> Self {
        Self::with_metric_capacity(MAX_METRIC_SERIES).expect("metric capacity is non-zero")
    }

    pub(super) fn with_metric_capacity(capacity: usize) -> Result<Self, RegistryError> {
        if capacity == 0 {
            return Err(RegistryError::ZeroMetricCapacity);
        }
        Ok(Self {
            max_metric_series: capacity,
            metrics: Mutex::new(BTreeMap::new()),
            requests: Mutex::new(None),
        })
    }

    pub(super) fn record(&self, event: &MetricEvent) -> Result<(), RegistryError> {
        event.validate().map_err(|_| RegistryError::InvalidMetric)?;
        let component = match event.name() {
            MetricName::ResolutionEventsAccepted => Component::Resolution,
            MetricName::WriterQueueDepth => Component::Telemetry,
            _ => return Err(RegistryError::InvalidMetric),
        };
        // 首期描述符仅允许一个 Component 标签，规范化为 typed key，不存任意标签。
        if event.labels().len() != 1
            || !event.labels().iter().all(|label| {
                label.key() == MetricLabelKey::Component
                    && matches!(label.value(), MetricLabelValue::Component(value) if *value == component)
            })
        {
            return Err(RegistryError::InvalidMetric);
        }
        let counter = match (event.name(), event.value()) {
            (MetricName::ResolutionEventsAccepted, MetricValue::Counter(_)) => true,
            (MetricName::WriterQueueDepth, MetricValue::Gauge(_)) => false,
            _ => return Err(RegistryError::MetricKindMismatch),
        };
        let key = MetricKey {
            name: event.name(),
            component,
        };
        let cell = {
            let mut metrics = lock_unpoisoned(&self.metrics);
            if let Some(cell) = metrics.get(&key) {
                cell.clone()
            } else {
                let reserved = if lock_unpoisoned(&self.requests).is_some() {
                    REQUEST_SERIES
                } else {
                    0
                };
                if metrics.len() + reserved >= self.max_metric_series {
                    return Err(RegistryError::MetricCapacityExhausted);
                }
                let cell = Arc::new(if counter {
                    MetricCell::Counter(AtomicU64::new(0))
                } else {
                    MetricCell::Gauge(AtomicI64::new(0))
                });
                metrics.insert(key, cell.clone());
                cell
            }
        };
        match (cell.as_ref(), event.value()) {
            (MetricCell::Counter(value), MetricValue::Counter(amount)) => value
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(amount)
                })
                .map(|_| ())
                .map_err(|_| RegistryError::CounterOverflow),
            (MetricCell::Gauge(value), MetricValue::Gauge(current)) => {
                value.store(current, Ordering::Release);
                Ok(())
            }
            _ => Err(RegistryError::MetricKindMismatch),
        }
    }

    /// 仅由 resolution 后台 dispatcher 调用，直接使用已冻结字段，不进入日志事件队列。
    pub(super) fn record_resolution(&self, event: &ResolutionEvent) -> Result<(), RegistryError> {
        let metrics = lock_unpoisoned(&self.metrics);
        let mut requests = lock_unpoisoned(&self.requests);
        if requests.is_none() && metrics.len() + REQUEST_SERIES > self.max_metric_series {
            return Err(RegistryError::MetricCapacityExhausted);
        }
        drop(metrics);
        // 一次完成事件同时更新各维度，任一溢出都不发布部分聚合结果。
        let mut next = requests.unwrap_or_default();
        next.latency.observe(
            event
                .duration_millis
                .checked_mul(1_000)
                .ok_or(RegistryError::CounterOverflow)?,
        )?;
        next.core_latency.observe(event.dns_core_duration_micros)?;
        let outcome = OUTCOMES
            .iter()
            .position(|value| *value == event.outcome)
            .ok_or(RegistryError::InvalidMetric)?;
        let cache = CACHE_STATUSES
            .iter()
            .position(|value| *value == event.cache_lookup_status)
            .ok_or(RegistryError::InvalidMetric)?;
        next.outcomes[outcome] = next.outcomes[outcome]
            .checked_add(1)
            .ok_or(RegistryError::CounterOverflow)?;
        next.cache[cache] = next.cache[cache]
            .checked_add(1)
            .ok_or(RegistryError::CounterOverflow)?;
        *requests = Some(next);
        Ok(())
    }

    pub(super) fn snapshot(&self) -> Vec<MetricSnapshot> {
        let mut snapshot: Vec<_> = lock_unpoisoned(&self.metrics)
            .iter()
            .map(|(key, cell)| MetricSnapshot {
                name: key.name,
                component: key.component,
                value: cell.value(),
                outcome: None,
                cache_status: None,
                histogram: None,
            })
            .collect();
        if let Some(requests) = *lock_unpoisoned(&self.requests) {
            for (name, histogram) in [
                (MetricName::RequestLatency, requests.latency),
                (MetricName::DnsCoreLatency, requests.core_latency),
            ] {
                snapshot.push(MetricSnapshot {
                    name,
                    component: Component::Resolution,
                    value: MetricValue::Counter(histogram.count),
                    outcome: None,
                    cache_status: None,
                    histogram: Some(histogram),
                });
            }
            for (outcome, count) in OUTCOMES.into_iter().zip(requests.outcomes) {
                snapshot.push(MetricSnapshot {
                    name: MetricName::RequestsTotal,
                    component: Component::Resolution,
                    value: MetricValue::Counter(count),
                    outcome: Some(outcome),
                    cache_status: None,
                    histogram: None,
                });
            }
            for (cache, count) in CACHE_STATUSES.into_iter().zip(requests.cache) {
                snapshot.push(MetricSnapshot {
                    name: MetricName::CacheOperations,
                    component: Component::Resolution,
                    value: MetricValue::Counter(count),
                    outcome: None,
                    cache_status: Some(cache),
                    histogram: None,
                });
            }
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::telemetry::MetricLabel;

    pub(super) fn event(name: MetricName, value: MetricValue) -> MetricEvent {
        let component = if name == MetricName::WriterQueueDepth {
            Component::Telemetry
        } else {
            Component::Resolution
        };
        MetricEvent::new(
            name,
            vec![
                MetricLabel::new(
                    MetricLabelKey::Component,
                    MetricLabelValue::Component(component),
                )
                .unwrap(),
            ],
            value,
        )
        .unwrap()
    }

    #[test]
    fn concurrent_updates_are_atomic_and_gauges_replace() {
        let registry = Arc::new(ObservabilityRegistry::new());
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let registry = registry.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        registry
                            .record(&event(
                                MetricName::ResolutionEventsAccepted,
                                MetricValue::Counter(1),
                            ))
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        registry
            .record(&event(MetricName::WriterQueueDepth, MetricValue::Gauge(9)))
            .unwrap();
        registry
            .record(&event(MetricName::WriterQueueDepth, MetricValue::Gauge(1)))
            .unwrap();
        let snapshot = registry.snapshot();
        assert!(
            snapshot
                .iter()
                .any(|item| item.value == MetricValue::Counter(800))
        );
        assert!(
            snapshot
                .iter()
                .any(|item| item.value == MetricValue::Gauge(1))
        );
    }

    #[test]
    fn capacity_overflow_and_descriptors_are_checked_without_corrupting_values() {
        assert!(matches!(
            ObservabilityRegistry::with_metric_capacity(0),
            Err(RegistryError::ZeroMetricCapacity)
        ));
        let registry = ObservabilityRegistry::with_metric_capacity(1).unwrap();
        registry
            .record(&event(
                MetricName::ResolutionEventsAccepted,
                MetricValue::Counter(u64::MAX),
            ))
            .unwrap();
        assert_eq!(
            registry.record(&event(
                MetricName::ResolutionEventsAccepted,
                MetricValue::Counter(1)
            )),
            Err(RegistryError::CounterOverflow)
        );
        assert_eq!(
            registry.record(&event(MetricName::WriterQueueDepth, MetricValue::Gauge(0))),
            Err(RegistryError::MetricCapacityExhausted)
        );
        assert_eq!(
            registry.record(&event(
                MetricName::ResolutionEventsAccepted,
                MetricValue::Gauge(0)
            )),
            Err(RegistryError::MetricKindMismatch)
        );
        assert_eq!(
            registry.record(&event(MetricName::RequestsTotal, MetricValue::Counter(1))),
            Err(RegistryError::InvalidMetric)
        );
        let missing_labels = MetricEvent::new(
            MetricName::ResolutionEventsAccepted,
            vec![],
            MetricValue::Counter(1),
        )
        .unwrap();
        assert_eq!(
            registry.record(&missing_labels),
            Err(RegistryError::InvalidMetric)
        );
        assert_eq!(registry.snapshot()[0].value, MetricValue::Counter(u64::MAX));
    }
}
