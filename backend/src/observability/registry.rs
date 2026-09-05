//! 正式 typed metrics 的有界聚合；不再拥有独立事件队列或 health 状态。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ports::telemetry::{
    Component, MetricEvent, MetricLabelKey, MetricLabelValue, MetricName, MetricValue,
};

use super::lock_unpoisoned;

const MAX_METRIC_SERIES: usize = 128;

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
                if metrics.len() >= self.max_metric_series {
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

    pub(super) fn snapshot(&self) -> Vec<MetricSnapshot> {
        lock_unpoisoned(&self.metrics)
            .iter()
            .map(|(key, cell)| MetricSnapshot {
                name: key.name,
                component: key.component,
                value: cell.value(),
            })
            .collect()
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
