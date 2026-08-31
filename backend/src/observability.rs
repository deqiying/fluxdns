use std::{
    collections::{BTreeMap, VecDeque},
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
    WriterQueueDepth,
    WriterDropped,
    WriterFlushed,
    WriterFailed,
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
            Self::WriterQueueDepth => "writer_queue_depth",
            Self::WriterDropped => "writer_dropped",
            Self::WriterFlushed => "writer_flushed",
            Self::WriterFailed => "writer_failed",
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

/// writer 只保存事件的低基数元数据，不保存 `TypedEvent::message`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferedEvent {
    name: EventName,
    component: Component,
    result: EventResult,
    occurred_at: Instant,
}

impl BufferedEvent {
    pub const fn name(self) -> EventName {
        self.name
    }

    pub const fn component(self) -> Component {
        self.component
    }

    pub const fn result(self) -> EventResult {
        self.result
    }

    pub const fn occurred_at(self) -> Instant {
        self.occurred_at
    }
}

/// sink 的最小契约。实现可以将元数据写入真实日志系统，但不得要求 writer 保存原始 message。
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EventSinkError {
    #[error("event sink write failed")]
    WriteFailed,
}

pub trait EventSink {
    fn write(&mut self, event: BufferedEvent) -> Result<(), EventSinkError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmitResult {
    Queued,
    DroppedFull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EventWriterError {
    #[error("event writer capacity must be greater than zero")]
    ZeroCapacity,
    #[error("event writer is closed")]
    Closed,
    #[error("event could not be recorded in observability registry: {0}")]
    Registry(#[from] RegistryError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriterSummary {
    accepted: u64,
    flushed: u64,
    dropped: u64,
    failed: u64,
    discarded: u64,
    pending: usize,
    closed: bool,
}

impl WriterSummary {
    pub const fn accepted(self) -> u64 {
        self.accepted
    }

    pub const fn flushed(self) -> u64 {
        self.flushed
    }

    pub const fn dropped(self) -> u64 {
        self.dropped
    }

    pub const fn failed(self) -> u64 {
        self.failed
    }

    pub const fn discarded(self) -> u64 {
        self.discarded
    }

    pub const fn pending(self) -> usize {
        self.pending
    }

    pub const fn closed(self) -> bool {
        self.closed
    }
}

struct QueuedEvent {
    event: BufferedEvent,
}

struct WriterState {
    queue: VecDeque<QueuedEvent>,
    accepted: u64,
    flushed: u64,
    dropped: u64,
    failed: u64,
    discarded: u64,
    closed: bool,
}

/// 进程内、非异步且有界的事件 writer buffer。
pub struct EventWriter {
    capacity: usize,
    registry: Arc<ObservabilityRegistry>,
    state: Mutex<WriterState>,
}

impl EventWriter {
    pub fn new(
        registry: Arc<ObservabilityRegistry>,
        capacity: usize,
    ) -> Result<Self, EventWriterError> {
        if capacity == 0 {
            return Err(EventWriterError::ZeroCapacity);
        }

        let queue_key = MetricKey::new(MetricName::WriterQueueDepth, Component::Observability);
        registry.set_gauge(queue_key, 0)?;

        Ok(Self {
            capacity,
            registry,
            state: Mutex::new(WriterState {
                queue: VecDeque::with_capacity(capacity),
                accepted: 0,
                flushed: 0,
                dropped: 0,
                failed: 0,
                discarded: 0,
                closed: false,
            }),
        })
    }

    pub fn emit(
        &self,
        event: TypedEvent,
        occurred_at: Instant,
    ) -> Result<EmitResult, EventWriterError> {
        let mut state = lock_unpoisoned(&self.state);
        if state.closed {
            return Err(EventWriterError::Closed);
        }
        if state.queue.len() == self.capacity {
            state.dropped = state.dropped.saturating_add(1);
            self.record_writer_counter(MetricName::WriterDropped);
            return Ok(EmitResult::DroppedFull);
        }

        self.registry.record_event(event, occurred_at)?;
        state.queue.push_back(QueuedEvent {
            event: BufferedEvent {
                name: event.name,
                component: event.component,
                result: event.result,
                occurred_at,
            },
        });
        state.accepted = state.accepted.saturating_add(1);
        self.update_queue_depth(state.queue.len());
        Ok(EmitResult::Queued)
    }

    pub fn flush(&self) -> WriterSummary {
        self.flush_with(&mut NoopSink)
    }

    pub fn flush_with<S: EventSink>(&self, sink: &mut S) -> WriterSummary {
        let batch = {
            let mut state = lock_unpoisoned(&self.state);
            state.queue.drain(..).collect::<Vec<_>>()
        };
        let mut failed_events = Vec::new();

        for queued in batch {
            if sink.write(queued.event).is_ok() {
                let mut state = lock_unpoisoned(&self.state);
                state.flushed = state.flushed.saturating_add(1);
                self.record_writer_counter(MetricName::WriterFlushed);
            } else {
                self.record_writer_counter(MetricName::WriterFailed);
                failed_events.push(queued);
            }
        }

        let mut state = lock_unpoisoned(&self.state);
        if !failed_events.is_empty() {
            state.failed = state.failed.saturating_add(failed_events.len() as u64);
            for queued in failed_events.into_iter().rev() {
                if state.closed || state.queue.len() == self.capacity {
                    state.discarded = state.discarded.saturating_add(1);
                } else {
                    state.queue.push_front(queued);
                }
            }
        }
        self.update_queue_depth(state.queue.len());
        self.make_summary(&state)
    }

    pub fn shutdown(&self) -> WriterSummary {
        let mut state = lock_unpoisoned(&self.state);
        state.closed = true;
        state.discarded = state.discarded.saturating_add(state.queue.len() as u64);
        state.queue.clear();
        self.update_queue_depth(0);
        self.make_summary(&state)
    }

    pub fn summary(&self) -> WriterSummary {
        let state = lock_unpoisoned(&self.state);
        self.make_summary(&state)
    }

    fn make_summary(&self, state: &WriterState) -> WriterSummary {
        WriterSummary {
            accepted: state.accepted,
            flushed: state.flushed,
            dropped: state.dropped,
            failed: state.failed,
            discarded: state.discarded,
            pending: state.queue.len(),
            closed: state.closed,
        }
    }

    fn update_queue_depth(&self, depth: usize) {
        let _ = self.registry.set_gauge(
            MetricKey::new(MetricName::WriterQueueDepth, Component::Observability),
            depth as i64,
        );
    }

    fn record_writer_counter(&self, name: MetricName) {
        let _ = self
            .registry
            .increment_counter(MetricKey::new(name, Component::Observability), 1);
    }
}

struct NoopSink;

impl EventSink for NoopSink {
    fn write(&mut self, _event: BufferedEvent) -> Result<(), EventSinkError> {
        Ok(())
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
        BufferedEvent, Component, EmitResult, EventName, EventResult, EventSink, EventSinkError,
        EventWriter, EventWriterError, HealthState, LogLevel, MetricKey, MetricName, MetricValue,
        ObservabilityRegistry, RegistryError, Sensitive, TypedEvent, bootstrap_subscriber,
    };

    #[derive(Debug)]
    struct TestSink {
        fail: bool,
        writes: Vec<BufferedEvent>,
    }

    impl EventSink for TestSink {
        fn write(&mut self, event: BufferedEvent) -> Result<(), EventSinkError> {
            if self.fail {
                return Err(EventSinkError::WriteFailed);
            }
            self.writes.push(event);
            Ok(())
        }
    }

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

    #[test]
    fn writer_rejects_zero_capacity_and_drops_when_full() {
        let registry = Arc::new(ObservabilityRegistry::new());
        assert!(matches!(
            EventWriter::new(Arc::clone(&registry), 0),
            Err(EventWriterError::ZeroCapacity)
        ));

        let writer = EventWriter::new(Arc::clone(&registry), 1).unwrap();
        let event = TypedEvent::new(
            EventName::ComponentStateChange,
            Component::Observability,
            EventResult::Degraded,
            "secret.example.invalid must never be buffered",
        );
        assert_eq!(
            writer.emit(event, Instant::now()).unwrap(),
            EmitResult::Queued
        );
        assert_eq!(
            writer.emit(event, Instant::now()).unwrap(),
            EmitResult::DroppedFull
        );

        let summary = writer.summary();
        assert_eq!(summary.accepted(), 1);
        assert_eq!(summary.dropped(), 1);
        assert_eq!(summary.pending(), 1);
        assert!(!format!("{summary:?}").contains("secret.example.invalid"));
        assert_eq!(
            registry
                .read_metric(MetricKey::new(
                    MetricName::WriterDropped,
                    Component::Observability,
                ))
                .unwrap()
                .value(),
            MetricValue::Counter(1)
        );
        assert_eq!(
            registry
                .read_metric(MetricKey::new(
                    MetricName::WriterQueueDepth,
                    Component::Observability,
                ))
                .unwrap()
                .value(),
            MetricValue::Gauge(1)
        );
    }

    #[test]
    fn writer_flushes_metadata_without_retaining_message() {
        let registry = Arc::new(ObservabilityRegistry::new());
        let writer = EventWriter::new(Arc::clone(&registry), 2).unwrap();
        let event = TypedEvent::new(
            EventName::ScaffoldReady,
            Component::Application,
            EventResult::Success,
            "secret.example.invalid must never reach a sink event",
        );
        writer.emit(event, Instant::now()).unwrap();

        let mut sink = TestSink {
            fail: false,
            writes: Vec::new(),
        };
        let summary = writer.flush_with(&mut sink);
        assert_eq!(summary.accepted(), 1);
        assert_eq!(summary.flushed(), 1);
        assert_eq!(summary.pending(), 0);
        assert_eq!(sink.writes.len(), 1);
        assert_eq!(sink.writes[0].name(), EventName::ScaffoldReady);
        assert!(!format!("{sink:?}").contains("secret.example.invalid"));
    }

    #[test]
    fn writer_requeues_failed_flush_and_reports_retryable_state() {
        let registry = Arc::new(ObservabilityRegistry::new());
        let writer = EventWriter::new(Arc::clone(&registry), 2).unwrap();
        writer
            .emit(
                TypedEvent::new(
                    EventName::ScaffoldReady,
                    Component::Application,
                    EventResult::Success,
                    "not retained",
                ),
                Instant::now(),
            )
            .unwrap();

        let mut failing_sink = TestSink {
            fail: true,
            writes: Vec::new(),
        };
        let failed = writer.flush_with(&mut failing_sink);
        assert_eq!(failed.failed(), 1);
        assert_eq!(failed.pending(), 1);
        assert_eq!(failed.flushed(), 0);

        let mut working_sink = TestSink {
            fail: false,
            writes: Vec::new(),
        };
        let recovered = writer.flush_with(&mut working_sink);
        assert_eq!(recovered.failed(), 1);
        assert_eq!(recovered.flushed(), 1);
        assert_eq!(recovered.pending(), 0);
        assert_eq!(
            registry
                .read_metric(MetricKey::new(
                    MetricName::WriterFailed,
                    Component::Observability,
                ))
                .unwrap()
                .value(),
            MetricValue::Counter(1)
        );
    }

    #[test]
    fn writer_shutdown_discards_pending_items_and_rejects_new_events() {
        let registry = Arc::new(ObservabilityRegistry::new());
        let writer = EventWriter::new(Arc::clone(&registry), 1).unwrap();
        writer
            .emit(
                TypedEvent::new(
                    EventName::ScaffoldReady,
                    Component::Application,
                    EventResult::Success,
                    "not retained",
                ),
                Instant::now(),
            )
            .unwrap();

        let summary = writer.shutdown();
        assert!(summary.closed());
        assert_eq!(summary.discarded(), 1);
        assert_eq!(summary.pending(), 0);
        assert_eq!(
            writer
                .emit(
                    TypedEvent::new(
                        EventName::ScaffoldReady,
                        Component::Application,
                        EventResult::Success,
                        "not retained",
                    ),
                    Instant::now(),
                )
                .unwrap_err(),
            EventWriterError::Closed
        );
        assert_eq!(writer.shutdown().discarded(), 1);
    }
}
