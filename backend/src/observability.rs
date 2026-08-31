use std::{
    collections::BTreeMap,
    fmt,
    str::FromStr,
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use tracing::Subscriber;

/// 构建仅写 stderr、固定为 INFO 及以上的阶段 1 bootstrap subscriber。
pub fn bootstrap_subscriber() -> impl Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .with_writer(std::io::stderr)
        .finish()
}

/// 安装进程级 bootstrap subscriber。
pub fn init_bootstrap() -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(bootstrap_subscriber())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl FromStr for LogLevel {
    type Err = ParseLogLevelError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.eq_ignore_ascii_case("trace") {
            Ok(Self::Trace)
        } else if input.eq_ignore_ascii_case("debug") {
            Ok(Self::Debug)
        } else if input.eq_ignore_ascii_case("info") {
            Ok(Self::Info)
        } else if input.eq_ignore_ascii_case("warn") {
            Ok(Self::Warn)
        } else if input.eq_ignore_ascii_case("error") {
            Ok(Self::Error)
        } else {
            Err(ParseLogLevelError)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("日志级别必须是 trace、debug、info、warn 或 error")]
pub struct ParseLogLevelError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthState {
    Healthy,
    Degraded,
    Failed,
    Stopping,
}

impl HealthState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Stopping => "stopping",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventName {
    ScaffoldReady,
    ComponentStateChange,
}

impl EventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScaffoldReady => "scaffold_ready",
            Self::ComponentStateChange => "component.state_change",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Component {
    Application,
    Observability,
    DnsCore,
    Ports,
}

impl Component {
    const ALL: [Self; 4] = [
        Self::Application,
        Self::Observability,
        Self::DnsCore,
        Self::Ports,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Observability => "observability",
            Self::DnsCore => "dns_core",
            Self::Ports => "ports",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Application => 0,
            Self::Observability => 1,
            Self::DnsCore => 2,
            Self::Ports => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventResult {
    Success,
    Degraded,
    Failure,
}

impl EventResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Degraded => "degraded",
            Self::Failure => "failure",
        }
    }
}

/// 阶段 1 的最小 typed event 契约。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TypedEvent {
    pub name: EventName,
    pub component: Component,
    pub result: EventResult,
    pub message: &'static str,
}

impl TypedEvent {
    pub const fn new(
        name: EventName,
        component: Component,
        result: EventResult,
        message: &'static str,
    ) -> Self {
        Self {
            name,
            component,
            result,
            message,
        }
    }
}

/// Registry 只接受固定集合中的 metric 名称，避免自由字符串带来的高基数和敏感字段。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MetricName {
    EventsTotal,
    EventsDegraded,
    EventsFailed,
    RetriesTotal,
    PersistenceGaps,
    ActiveRequests,
}

impl MetricName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EventsTotal => "events_total",
            Self::EventsDegraded => "events_degraded",
            Self::EventsFailed => "events_failed",
            Self::RetriesTotal => "retries_total",
            Self::PersistenceGaps => "persistence_gaps",
            Self::ActiveRequests => "active_requests",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MetricKey {
    name: MetricName,
    component: Component,
}

impl MetricKey {
    pub const fn new(name: MetricName, component: Component) -> Self {
        Self { name, component }
    }

    pub const fn name(self) -> MetricName {
        self.name
    }

    pub const fn component(self) -> Component {
        self.component
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricValue {
    Counter(u64),
    Gauge(i64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetricSnapshot {
    key: MetricKey,
    value: MetricValue,
}

impl MetricSnapshot {
    pub const fn key(self) -> MetricKey {
        self.key
    }

    pub const fn value(self) -> MetricValue {
        self.value
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RegistryError {
    #[error("metric series capacity must be greater than zero")]
    ZeroMetricCapacity,
    #[error("metric series capacity has been exhausted")]
    MetricCapacityExhausted,
    #[error("metric key is already registered with another value kind")]
    MetricKindMismatch,
    #[error("metric counter overflow")]
    CounterOverflow,
    #[error("metric gauge overflow")]
    GaugeOverflow,
    #[error("health retry counter overflow")]
    RetryOverflow,
}

enum MetricCell {
    Counter(AtomicU64),
    Gauge(AtomicI64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthSnapshot {
    component: Component,
    state: HealthState,
    first_seen: Instant,
    last_changed: Instant,
    last_success: Option<Instant>,
    retry_count: u64,
    persistence_gap: bool,
}

impl HealthSnapshot {
    pub const fn component(self) -> Component {
        self.component
    }

    pub const fn state(self) -> HealthState {
        self.state
    }

    pub const fn first_seen(self) -> Instant {
        self.first_seen
    }

    pub const fn last_changed(self) -> Instant {
        self.last_changed
    }

    pub const fn last_success(self) -> Option<Instant> {
        self.last_success
    }

    pub const fn retry_count(self) -> u64 {
        self.retry_count
    }

    pub const fn persistence_gap(self) -> bool {
        self.persistence_gap
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrySnapshot {
    metrics: Vec<MetricSnapshot>,
    health: Vec<HealthSnapshot>,
}

impl RegistrySnapshot {
    pub fn metrics(&self) -> &[MetricSnapshot] {
        &self.metrics
    }

    pub fn health(&self) -> &[HealthSnapshot] {
        &self.health
    }
}

struct HealthRecord {
    state: HealthState,
    first_seen: Instant,
    last_changed: Instant,
    last_success: Option<Instant>,
    retry_count: u64,
    persistence_gap: bool,
}

impl HealthRecord {
    fn snapshot(&self, component: Component) -> HealthSnapshot {
        HealthSnapshot {
            component,
            state: self.state,
            first_seen: self.first_seen,
            last_changed: self.last_changed,
            last_success: self.last_success,
            retry_count: self.retry_count,
            persistence_gap: self.persistence_gap,
        }
    }
}

/// 进程内有界 registry。所有可变状态只包含固定组件和固定 metric key。
pub struct ObservabilityRegistry {
    max_metric_series: usize,
    metrics: Mutex<BTreeMap<MetricKey, Arc<MetricCell>>>,
    health: Mutex<[Option<HealthRecord>; 4]>,
}

impl ObservabilityRegistry {
    pub fn new() -> Self {
        Self::with_metric_capacity(24).expect("default metric capacity is non-zero")
    }

    pub fn with_metric_capacity(max_metric_series: usize) -> Result<Self, RegistryError> {
        if max_metric_series == 0 {
            return Err(RegistryError::ZeroMetricCapacity);
        }
        Ok(Self {
            max_metric_series,
            metrics: Mutex::new(BTreeMap::new()),
            health: Mutex::new([None, None, None, None]),
        })
    }

    pub fn increment_counter(&self, key: MetricKey, amount: u64) -> Result<(), RegistryError> {
        let cell = self.metric_cell(key, MetricKind::Counter)?;
        match cell.as_ref() {
            MetricCell::Counter(value) => value
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(amount)
                })
                .map(|_| ())
                .map_err(|_| RegistryError::CounterOverflow),
            MetricCell::Gauge(_) => Err(RegistryError::MetricKindMismatch),
        }
    }

    pub fn set_gauge(&self, key: MetricKey, value: i64) -> Result<(), RegistryError> {
        let cell = self.metric_cell(key, MetricKind::Gauge)?;
        match cell.as_ref() {
            MetricCell::Gauge(current) => {
                current.store(value, Ordering::Release);
                Ok(())
            }
            MetricCell::Counter(_) => Err(RegistryError::MetricKindMismatch),
        }
    }

    pub fn add_gauge(&self, key: MetricKey, amount: i64) -> Result<(), RegistryError> {
        let cell = self.metric_cell(key, MetricKind::Gauge)?;
        match cell.as_ref() {
            MetricCell::Gauge(value) => value
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(amount)
                })
                .map(|_| ())
                .map_err(|_| RegistryError::GaugeOverflow),
            MetricCell::Counter(_) => Err(RegistryError::MetricKindMismatch),
        }
    }

    pub fn read_metric(&self, key: MetricKey) -> Option<MetricSnapshot> {
        let cell = lock_unpoisoned(&self.metrics).get(&key).cloned()?;
        Some(MetricSnapshot {
            key,
            value: read_metric_value(&cell),
        })
    }

    pub fn update_health(
        &self,
        component: Component,
        state: HealthState,
        at: Instant,
    ) -> Result<(), RegistryError> {
        let mut health = lock_unpoisoned(&self.health);
        let slot = &mut health[component.index()];
        match slot {
            Some(record) => {
                if record.state != state {
                    record.state = state;
                    record.last_changed = at;
                }
                if state == HealthState::Healthy {
                    record.last_success = Some(at);
                }
            }
            None => {
                *slot = Some(HealthRecord {
                    state,
                    first_seen: at,
                    last_changed: at,
                    last_success: (state == HealthState::Healthy).then_some(at),
                    retry_count: 0,
                    persistence_gap: false,
                });
            }
        }
        Ok(())
    }

    pub fn record_retry(&self, component: Component) -> Result<(), RegistryError> {
        let mut health = lock_unpoisoned(&self.health);
        if health[component.index()]
            .as_ref()
            .is_some_and(|record| record.retry_count == u64::MAX)
        {
            return Err(RegistryError::RetryOverflow);
        }
        self.increment_counter(MetricKey::new(MetricName::RetriesTotal, component), 1)?;
        let record = health[component.index()].get_or_insert_with(|| {
            let now = Instant::now();
            HealthRecord {
                state: HealthState::Degraded,
                first_seen: now,
                last_changed: now,
                last_success: None,
                retry_count: 0,
                persistence_gap: false,
            }
        });
        record.retry_count += 1;
        Ok(())
    }

    pub fn set_persistence_gap(&self, component: Component, active: bool) {
        let mut health = lock_unpoisoned(&self.health);
        let record = health[component.index()].get_or_insert_with(|| {
            let now = Instant::now();
            HealthRecord {
                state: HealthState::Degraded,
                first_seen: now,
                last_changed: now,
                last_success: None,
                retry_count: 0,
                persistence_gap: false,
            }
        });
        record.persistence_gap = active;
    }

    /// Typed event 只参与固定指标和健康状态更新，message 永远不进入 snapshot。
    pub fn record_event(&self, event: TypedEvent, at: Instant) -> Result<(), RegistryError> {
        self.update_health(event.component, health_for_result(event.result), at)?;
        self.increment_counter(MetricKey::new(MetricName::EventsTotal, event.component), 1)?;
        match event.result {
            EventResult::Success => {}
            EventResult::Degraded => {
                self.increment_counter(
                    MetricKey::new(MetricName::EventsDegraded, event.component),
                    1,
                )?;
            }
            EventResult::Failure => {
                self.increment_counter(
                    MetricKey::new(MetricName::EventsFailed, event.component),
                    1,
                )?;
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> RegistrySnapshot {
        let metrics = lock_unpoisoned(&self.metrics)
            .iter()
            .map(|(&key, cell)| MetricSnapshot {
                key,
                value: read_metric_value(cell),
            })
            .collect();
        let health = lock_unpoisoned(&self.health)
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                record
                    .as_ref()
                    .map(|record| record.snapshot(Component::ALL[index]))
            })
            .collect();
        RegistrySnapshot { metrics, health }
    }

    fn metric_cell(
        &self,
        key: MetricKey,
        kind: MetricKind,
    ) -> Result<Arc<MetricCell>, RegistryError> {
        let mut metrics = lock_unpoisoned(&self.metrics);
        if let Some(cell) = metrics.get(&key) {
            if metric_kind(cell) != kind {
                return Err(RegistryError::MetricKindMismatch);
            }
            return Ok(Arc::clone(cell));
        }
        if metrics.len() >= self.max_metric_series {
            return Err(RegistryError::MetricCapacityExhausted);
        }
        let cell = Arc::new(match kind {
            MetricKind::Counter => MetricCell::Counter(AtomicU64::new(0)),
            MetricKind::Gauge => MetricCell::Gauge(AtomicI64::new(0)),
        });
        metrics.insert(key, Arc::clone(&cell));
        Ok(cell)
    }
}

impl Default for ObservabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MetricKind {
    Counter,
    Gauge,
}

fn metric_kind(cell: &MetricCell) -> MetricKind {
    match cell {
        MetricCell::Counter(_) => MetricKind::Counter,
        MetricCell::Gauge(_) => MetricKind::Gauge,
    }
}

fn read_metric_value(cell: &MetricCell) -> MetricValue {
    match cell {
        MetricCell::Counter(value) => MetricValue::Counter(value.load(Ordering::Acquire)),
        MetricCell::Gauge(value) => MetricValue::Gauge(value.load(Ordering::Acquire)),
    }
}

fn health_for_result(result: EventResult) -> HealthState {
    match result {
        EventResult::Success => HealthState::Healthy,
        EventResult::Degraded => HealthState::Degraded,
        EventResult::Failure => HealthState::Failed,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 包装敏感值，并在所有常规格式化路径中固定脱敏。
#[derive(Clone, Copy, Default)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Sensitive([REDACTED])")
    }
}

impl<T> fmt::Display for Sensitive<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    use super::{
        Component, EventName, EventResult, HealthState, LogLevel, MetricKey, MetricName,
        MetricValue, ObservabilityRegistry, RegistryError, Sensitive, TypedEvent,
        bootstrap_subscriber,
    };

    #[test]
    fn bootstrap_subscriber_can_be_built() {
        let subscriber = bootstrap_subscriber();
        let _dispatch = tracing::Dispatch::new(subscriber);
    }

    #[test]
    fn log_level_parsing_is_case_insensitive_and_strict() {
        assert_eq!(LogLevel::from_str("trace"), Ok(LogLevel::Trace));
        assert_eq!(LogLevel::from_str("DEBUG"), Ok(LogLevel::Debug));
        assert_eq!(LogLevel::from_str("Info"), Ok(LogLevel::Info));
        assert_eq!(LogLevel::from_str("WARN"), Ok(LogLevel::Warn));
        assert_eq!(LogLevel::from_str("error"), Ok(LogLevel::Error));
        assert!(LogLevel::from_str("warning").is_err());
        assert!(LogLevel::from_str(" info ").is_err());
    }

    #[test]
    fn sensitive_value_never_appears_in_debug_or_display() {
        let secret = "do-not-log-this-value";
        let sensitive = Sensitive::new(secret);
        let debug = format!("{sensitive:?}");
        let display = format!("{sensitive}");
        let derived_debug = format!("{:?}", Some(sensitive));

        assert!(!debug.contains(secret));
        assert!(!display.contains(secret));
        assert!(!derived_debug.contains(secret));
        assert_eq!(debug, "Sensitive([REDACTED])");
        assert_eq!(display, "[REDACTED]");
        assert_eq!(derived_debug, "Some(Sensitive([REDACTED]))");
    }

    #[test]
    fn health_state_is_idempotent_and_recovers_with_timestamps() {
        let registry = ObservabilityRegistry::new();
        let first = Instant::now();
        let failed = first + Duration::from_secs(1);
        let recovered = failed + Duration::from_secs(1);

        registry
            .update_health(Component::DnsCore, HealthState::Failed, first)
            .unwrap();
        registry
            .update_health(Component::DnsCore, HealthState::Failed, failed)
            .unwrap();
        let unchanged = registry.snapshot().health()[0];
        assert_eq!(unchanged.state(), HealthState::Failed);
        assert_eq!(unchanged.first_seen(), first);
        assert_eq!(unchanged.last_changed(), first);

        registry
            .record_retry(Component::DnsCore)
            .expect("retry is bounded");
        registry.set_persistence_gap(Component::DnsCore, true);
        registry
            .update_health(Component::DnsCore, HealthState::Healthy, recovered)
            .unwrap();

        let snapshot = registry.snapshot();
        let health = snapshot.health()[0];
        assert_eq!(health.state(), HealthState::Healthy);
        assert_eq!(health.first_seen(), first);
        assert_eq!(health.last_changed(), recovered);
        assert_eq!(health.last_success(), Some(recovered));
        assert_eq!(health.retry_count(), 1);
        assert!(health.persistence_gap());
    }

    #[test]
    fn typed_event_updates_fixed_metrics_without_retaining_message() {
        let registry = ObservabilityRegistry::new();
        let event = TypedEvent::new(
            EventName::ComponentStateChange,
            Component::Ports,
            EventResult::Failure,
            "secret.example.invalid should never enter a snapshot",
        );

        registry.record_event(event, Instant::now()).unwrap();
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.metrics().len(), 2);
        assert!(snapshot.metrics().iter().any(|metric| metric.key()
            == MetricKey::new(MetricName::EventsTotal, Component::Ports)
            && metric.value() == MetricValue::Counter(1)));
        assert!(snapshot.metrics().iter().any(|metric| {
            metric.key() == MetricKey::new(MetricName::EventsFailed, Component::Ports)
                && metric.value() == MetricValue::Counter(1)
        }));
        assert!(!format!("{snapshot:?}").contains("secret.example.invalid"));
    }

    #[test]
    fn metric_updates_are_atomic_and_kind_checked() {
        let registry = Arc::new(ObservabilityRegistry::new());
        let key = MetricKey::new(MetricName::RetriesTotal, Component::DnsCore);
        let workers = (0..8)
            .map(|_| {
                let registry = Arc::clone(&registry);
                thread::spawn(move || {
                    for _ in 0..100 {
                        registry.increment_counter(key, 1).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            registry.read_metric(key).unwrap().value(),
            MetricValue::Counter(800)
        );

        let gauge = MetricKey::new(MetricName::ActiveRequests, Component::Application);
        registry.set_gauge(gauge, 2).unwrap();
        registry.add_gauge(gauge, -1).unwrap();
        assert_eq!(
            registry.read_metric(gauge).unwrap().value(),
            MetricValue::Gauge(1)
        );
        assert_eq!(
            registry.set_gauge(key, 3).unwrap_err(),
            RegistryError::MetricKindMismatch
        );
    }

    #[test]
    fn metric_capacity_and_overflow_are_rejected_without_corrupting_values() {
        assert!(matches!(
            ObservabilityRegistry::with_metric_capacity(0),
            Err(RegistryError::ZeroMetricCapacity)
        ));

        let registry = ObservabilityRegistry::with_metric_capacity(1).unwrap();
        let first = MetricKey::new(MetricName::EventsTotal, Component::Application);
        let second = MetricKey::new(MetricName::EventsTotal, Component::Ports);
        registry.increment_counter(first, u64::MAX).unwrap();
        assert_eq!(
            registry.increment_counter(first, 1).unwrap_err(),
            RegistryError::CounterOverflow
        );
        assert_eq!(
            registry.increment_counter(second, 1).unwrap_err(),
            RegistryError::MetricCapacityExhausted
        );
        assert_eq!(
            registry.read_metric(first).unwrap().value(),
            MetricValue::Counter(u64::MAX)
        );

        let gauge = MetricKey::new(MetricName::ActiveRequests, Component::Application);
        let registry = ObservabilityRegistry::new();
        registry.set_gauge(gauge, i64::MAX).unwrap();
        assert_eq!(
            registry.add_gauge(gauge, 1).unwrap_err(),
            RegistryError::GaugeOverflow
        );
        assert_eq!(
            registry.read_metric(gauge).unwrap().value(),
            MetricValue::Gauge(i64::MAX)
        );
    }
}
