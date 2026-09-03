use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
    str::FromStr,
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Instant,
};

use crate::dns::Deadline;
use crate::ports::telemetry::{
    Component as TelemetryComponent, ComponentHealthEvent, ComponentHealthState,
    EventName as TelemetryEventName, HealthSink, LogEvent, LogLevel as TelemetryLogLevel, LogSink,
    MetricEvent, MetricsSink, OutcomeClass, TelemetryFlushSummary,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};

static BOOTSTRAP_OUTPUT: OnceLock<Arc<Mutex<OutputTarget>>> = OnceLock::new();
type FilteredRegistry = tracing_subscriber::layer::Layered<
    tracing_subscriber::reload::Layer<
        tracing_subscriber::filter::LevelFilter,
        tracing_subscriber::Registry,
    >,
    tracing_subscriber::Registry,
>;
type ReloadableTracingLayer = Box<dyn Layer<FilteredRegistry> + Send + Sync + 'static>;
static BOOTSTRAP_LAYER: OnceLock<
    tracing_subscriber::reload::Handle<ReloadableTracingLayer, FilteredRegistry>,
> = OnceLock::new();
static BOOTSTRAP_FILTER: OnceLock<
    tracing_subscriber::reload::Handle<
        tracing_subscriber::filter::LevelFilter,
        tracing_subscriber::Registry,
    >,
> = OnceLock::new();

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
    let output = BOOTSTRAP_OUTPUT
        .get_or_init(|| Arc::new(Mutex::new(OutputTarget::Stderr)))
        .clone();
    let (filter, filter_handle) =
        tracing_subscriber::reload::Layer::new(tracing_subscriber::filter::LevelFilter::INFO);
    let _ = BOOTSTRAP_FILTER.set(filter_handle);
    let (layer, layer_handle) = tracing_subscriber::reload::Layer::new(Box::new(
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(move || SharedOutputWriter(Arc::clone(&output))),
    )
        as ReloadableTracingLayer);
    let _ = BOOTSTRAP_LAYER.set(layer_handle);
    use tracing_subscriber::layer::SubscriberExt as _;
    let subscriber = tracing_subscriber::registry().with(filter).with(layer);
    tracing::subscriber::set_global_default(subscriber)
}

/// 在 bootstrap subscriber 已安装后切换到配置指定的真实输出。
///
/// 级别过滤由可 reload 的 bootstrap handle 控制；`enable=false` 时丢弃普通日志，
/// Application 的 fatal 退出信息仍由进程边界直接写 stderr。
pub fn configure_final_output(
    enable: bool,
    path: impl AsRef<Path>,
    level: LogLevel,
) -> io::Result<()> {
    let output = BOOTSTRAP_OUTPUT.get().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "bootstrap output is not initialized",
        )
    })?;
    let target = if !enable {
        OutputTarget::Sink
    } else {
        OutputTarget::File(OpenOptions::new().create(true).append(true).open(path)?)
    };
    *lock_unpoisoned(output) = target;
    let filter = BOOTSTRAP_FILTER.get().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "bootstrap filter is not initialized",
        )
    })?;
    filter
        .reload(if enable {
            level.as_filter()
        } else {
            tracing_subscriber::filter::LevelFilter::OFF
        })
        .map_err(|_| io::Error::other("bootstrap filter reload failed"))?;
    Ok(())
}

/// 将进程级 subscriber 的输出层切换为 typed telemetry layer。
pub fn install_final_tracing(
    writer: Arc<TelemetryWriter>,
) -> Result<(), TelemetryRuntimeBuildError> {
    let handle = BOOTSTRAP_LAYER
        .get()
        .ok_or(TelemetryRuntimeBuildError::BootstrapNotInitialized)?;
    handle
        .reload(Box::new(TypedTracingLayer { writer }) as ReloadableTracingLayer)
        .map_err(|_| TelemetryRuntimeBuildError::FinalLayerReload)
}

enum OutputTarget {
    Stderr,
    File(std::fs::File),
    Sink,
}

struct SharedOutputWriter(Arc<Mutex<OutputTarget>>);

impl Write for SharedOutputWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut *lock_unpoisoned(&self.0) {
            OutputTarget::Stderr => io::stderr().write(bytes),
            OutputTarget::File(file) => file.write(bytes),
            OutputTarget::Sink => Ok(bytes.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut *lock_unpoisoned(&self.0) {
            OutputTarget::Stderr => io::stderr().flush(),
            OutputTarget::File(file) => file.flush(),
            OutputTarget::Sink => Ok(()),
        }
    }
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

    const fn as_filter(self) -> tracing_subscriber::filter::LevelFilter {
        match self {
            Self::Trace => tracing_subscriber::filter::LevelFilter::TRACE,
            Self::Debug => tracing_subscriber::filter::LevelFilter::DEBUG,
            Self::Info => tracing_subscriber::filter::LevelFilter::INFO,
            Self::Warn => tracing_subscriber::filter::LevelFilter::WARN,
            Self::Error => tracing_subscriber::filter::LevelFilter::ERROR,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TelemetryWriterBuildError {
    #[error("telemetry writer capacity must be greater than zero")]
    ZeroCapacity,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TelemetryWriterStats {
    pending: usize,
    emitted: u64,
    dropped_low_priority: u64,
    failed: u64,
    closed: bool,
}

impl TelemetryWriterStats {
    pub const fn pending(self) -> usize {
        self.pending
    }

    pub const fn emitted(self) -> u64 {
        self.emitted
    }

    pub const fn dropped_low_priority(self) -> u64 {
        self.dropped_low_priority
    }

    pub const fn failed(self) -> u64 {
        self.failed
    }

    pub const fn closed(self) -> bool {
        self.closed
    }
}

/// Telemetry writer 的同步输出边界。
///
/// 输出端不得把原始 adapter 错误、query、header 或 secret 写回 `PortError`；writer
/// 只负责有界排队和生命周期，具体 tracing/file/stderr 输出由 typed layer 和 output
/// adapter 提供。
pub trait TelemetryOutput: Send + Sync {
    fn write_log(&self, event: &LogEvent) -> Result<(), PortError>;
    fn write_metric(&self, event: &MetricEvent) -> Result<(), PortError>;
    fn write_health(&self, event: &ComponentHealthEvent) -> Result<(), PortError>;
}

/// 将已经通过 typed telemetry 契约校验的事件写入真实文本输出。
///
/// 输出 adapter 不接收原始请求、header 或 adapter error；事件中的 request digest、
/// configured ID 和 message 仍按稳定字段写出。主输出失败时尝试写入 stderr fallback，
/// 两个输出都失败才返回安全的 `PortError`。
pub struct StructuredTelemetryOutput {
    writer: Mutex<Box<dyn Write + Send>>,
    fallback: Mutex<Option<Box<dyn Write + Send>>>,
}

impl StructuredTelemetryOutput {
    fn from_writer(writer: Box<dyn Write + Send>) -> Self {
        Self::from_writer_with_fallback(writer, None)
    }

    fn from_writer_with_fallback(
        writer: Box<dyn Write + Send>,
        fallback: Option<Box<dyn Write + Send>>,
    ) -> Self {
        Self {
            writer: Mutex::new(writer),
            fallback: Mutex::new(fallback),
        }
    }

    fn shared(output: Arc<Mutex<OutputTarget>>) -> Self {
        Self::from_writer_with_fallback(
            Box::new(SharedOutputWriter(output)),
            Some(Box::new(io::stderr())),
        )
    }

    pub fn file(path: impl AsRef<Path>) -> io::Result<Self> {
        let writer = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::from_writer_with_fallback(
            Box::new(writer),
            Some(Box::new(io::stderr())),
        ))
    }

    pub fn stderr() -> Self {
        Self::from_writer(Box::new(io::stderr()))
    }

    fn write_line(&self, line: &str) -> Result<(), PortError> {
        let primary_succeeded = {
            let mut writer = lock_unpoisoned(&self.writer);
            write_line_to(&mut **writer, line).is_ok()
        };
        if primary_succeeded {
            return Ok(());
        }

        let fallback_succeeded = {
            let mut fallback = lock_unpoisoned(&self.fallback);
            fallback
                .as_mut()
                .is_some_and(|writer| write_line_to(&mut **writer, line).is_ok())
        };
        if fallback_succeeded {
            return Ok(());
        }

        Err(PortError::new(
            PortErrorClass::Unavailable,
            "observability.telemetry.output",
        )
        .with_safe_context("output write failed"))
    }
}

fn write_line_to(writer: &mut dyn Write, line: &str) -> io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

impl TelemetryOutput for StructuredTelemetryOutput {
    fn write_log(&self, event: &LogEvent) -> Result<(), PortError> {
        self.write_line(&format!(
            "{{\"kind\":\"log\",\"occurred_at_ms\":{},\"level\":\"{}\",\"event\":{},\"component\":\"{}\",\"has_request_digest\":{},\"has_configured_id\":{},\"outcome\":\"{}\",\"runtime_revision\":{},\"message\":{}}}",
            system_time_millis(event.occurred_at),
            enum_name(event.level),
            json_escape(event.name.as_str()),
            enum_name(event.component),
            event.request_digest.is_some(),
            event.configured_id.is_some(),
            enum_name(event.outcome),
            event
                .runtime_revision
                .map_or_else(|| "null".to_owned(), |revision| revision.0.to_string()),
            json_escape(event.message),
        ))
    }

    fn write_metric(&self, event: &MetricEvent) -> Result<(), PortError> {
        let labels = event
            .labels()
            .iter()
            .map(metric_label)
            .collect::<Vec<_>>()
            .join(",");
        self.write_line(&format!(
            "{{\"kind\":\"metric\",\"name\":\"{}\",\"labels\":[{}],\"value\":\"{:?}\"}}",
            enum_name(event.name()),
            labels,
            event.value(),
        ))
    }

    fn write_health(&self, event: &ComponentHealthEvent) -> Result<(), PortError> {
        self.write_line(&format!(
            "{{\"kind\":\"health\",\"component\":\"{}\",\"state\":\"{}\",\"retry_count\":{},\"stale_age_micros\":{},\"persistence_gap\":{}}}",
            enum_name(event.component),
            enum_name(event.state),
            event.retry_count,
            event
                .stale_age_micros
                .map_or_else(|| "null".to_owned(), |value| value.to_string()),
            event.persistence_gap,
        ))
    }
}

fn metric_label(label: &crate::ports::telemetry::MetricLabel) -> String {
    use crate::ports::telemetry::MetricLabelValue;

    let value = match label.value() {
        MetricLabelValue::Component(value) => enum_name(*value),
        MetricLabelValue::Transport(value) => enum_name(*value),
        MetricLabelValue::Outcome(value) => enum_name(*value),
        MetricLabelValue::CacheStatus(value) => enum_name(*value),
        MetricLabelValue::ConfiguredId(value) => {
            format!("{}:{}", enum_name(value.kind()), value.as_str())
        }
    };
    format!(
        "{{\"key\":\"{}\",\"value\":{}}}",
        enum_name(label.key()),
        json_escape(&value),
    )
}

fn enum_name<T: fmt::Debug>(value: T) -> String {
    format!("{value:?}").to_ascii_lowercase()
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn system_time_millis(time: std::time::SystemTime) -> u128 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

enum TelemetryItem {
    Log(LogEvent),
    Metric(MetricEvent),
    Health(ComponentHealthEvent),
}

struct TelemetryWriterState {
    queue: VecDeque<TelemetryItem>,
    emitted: u64,
    dropped_low_priority: u64,
    failed: u64,
    closed: bool,
}

#[derive(Clone, Copy)]
struct TelemetryHealthRecord {
    state: ComponentHealthState,
    first_seen: Instant,
    last_changed: Instant,
    last_success: Option<Instant>,
    retry_count: u64,
    stale_age_micros: Option<u64>,
    persistence_gap: bool,
}

impl TelemetryHealthRecord {
    fn from_event(event: &ComponentHealthEvent) -> Self {
        Self {
            state: event.state,
            first_seen: event.first_seen,
            last_changed: event.last_changed,
            last_success: event.last_success,
            retry_count: event.retry_count,
            stale_age_micros: event.stale_age_micros,
            persistence_gap: event.persistence_gap,
        }
    }

    fn normalize(&self, mut event: ComponentHealthEvent) -> ComponentHealthEvent {
        event.first_seen = self.first_seen;
        if event.state == self.state || event.last_changed < self.last_changed {
            event.last_changed = self.last_changed;
        }
        event.last_success = match (self.last_success, event.last_success) {
            (Some(previous), Some(current)) => Some(previous.max(current)),
            (Some(previous), None) => Some(previous),
            (None, current) => current,
        };
        event.retry_count = event.retry_count.max(self.retry_count);
        event.stale_age_micros = if event.state == ComponentHealthState::Healthy {
            None
        } else {
            match (self.stale_age_micros, event.stale_age_micros) {
                (Some(previous), Some(current)) => Some(previous.max(current)),
                (Some(previous), None) => Some(previous),
                (None, current) => current,
            }
        };
        if self.persistence_gap && event.state != ComponentHealthState::Healthy {
            event.persistence_gap = true;
        }
        event
    }
}

/// 面向稳定 telemetry ports 的非阻塞有界 writer。
///
/// `emit`/`record`/`update` 只执行内存操作，不等待输出端；低优先级日志在拥塞时计数
/// 丢弃，warn/error 会优先淘汰已排队的低优先级日志，否则返回明确的容量错误。
/// 健康事件只保留每个组件一条有界 lifecycle record，用于归一化首次时间、最近成功、
/// 重试次数、stale age 和 persistence gap，不会把任意请求字段存入 writer 状态。
pub struct TelemetryWriter {
    capacity: usize,
    output: Arc<dyn TelemetryOutput>,
    state: Mutex<TelemetryWriterState>,
    flush_lock: Mutex<()>,
    health: Mutex<BTreeMap<TelemetryComponent, TelemetryHealthRecord>>,
}

impl TelemetryWriter {
    pub fn new(
        capacity: usize,
        output: Arc<dyn TelemetryOutput>,
    ) -> Result<Self, TelemetryWriterBuildError> {
        if capacity == 0 {
            return Err(TelemetryWriterBuildError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            output,
            state: Mutex::new(TelemetryWriterState {
                queue: VecDeque::with_capacity(capacity),
                emitted: 0,
                dropped_low_priority: 0,
                failed: 0,
                closed: false,
            }),
            flush_lock: Mutex::new(()),
            health: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn stats(&self) -> TelemetryWriterStats {
        let state = lock_unpoisoned(&self.state);
        TelemetryWriterStats {
            pending: state.queue.len(),
            emitted: state.emitted,
            dropped_low_priority: state.dropped_low_priority,
            failed: state.failed,
            closed: state.closed,
        }
    }

    pub fn shutdown(&self, deadline: Deadline) -> Result<TelemetryFlushSummary, PortError> {
        {
            let mut state = lock_unpoisoned(&self.state);
            state.closed = true;
        }
        self.flush_now(deadline)
    }

    fn enqueue(&self, item: TelemetryItem, low_priority: bool) -> Result<(), PortError> {
        let mut state = lock_unpoisoned(&self.state);
        if state.closed {
            return Err(PortError::new(
                PortErrorClass::Unavailable,
                "observability.telemetry.enqueue",
            )
            .with_safe_context("writer closed"));
        }

        if state.queue.len() < self.capacity {
            state.queue.push_back(item);
            return Ok(());
        }

        if low_priority {
            state.dropped_low_priority = state.dropped_low_priority.saturating_add(1);
            return Ok(());
        }

        if let Some(index) = state.queue.iter().position(TelemetryItem::is_low_priority) {
            let _ = state.queue.remove(index);
            state.dropped_low_priority = state.dropped_low_priority.saturating_add(1);
            state.queue.push_back(item);
            return Ok(());
        }

        Err(PortError::new(
            PortErrorClass::ResourceExhausted,
            "observability.telemetry.enqueue",
        )
        .with_safe_context("high-priority queue full"))
    }

    fn flush_now(&self, deadline: Deadline) -> Result<TelemetryFlushSummary, PortError> {
        let _flush_guard = lock_unpoisoned(&self.flush_lock);
        loop {
            if deadline.is_expired(Instant::now()) {
                return Err(PortError::new(
                    PortErrorClass::Timeout,
                    "observability.telemetry.flush",
                ));
            }

            let item = {
                let mut state = lock_unpoisoned(&self.state);
                state.queue.pop_front()
            };
            let Some(item) = item else {
                let state = lock_unpoisoned(&self.state);
                let summary = TelemetryFlushSummary {
                    emitted: state.emitted,
                    dropped_low_priority: state.dropped_low_priority,
                    failed: state.failed,
                };
                drop(state);
                self.record_output_recovery(Instant::now());
                return Ok(summary);
            };

            let result = match &item {
                TelemetryItem::Log(event) => self.output.write_log(event),
                TelemetryItem::Metric(event) => self.output.write_metric(event),
                TelemetryItem::Health(event) => self.output.write_health(event),
            };
            match result {
                Ok(()) => {
                    let mut state = lock_unpoisoned(&self.state);
                    state.emitted = state.emitted.saturating_add(1);
                }
                Err(error) => {
                    let mut state = lock_unpoisoned(&self.state);
                    state.failed = state.failed.saturating_add(1);
                    state.queue.push_front(item);
                    drop(state);
                    self.record_output_failure(Instant::now());
                    return Err(error);
                }
            }
        }
    }

    /// 在输出端与 fallback 均失败时记录进程内最终状态，不递归写入故障输出。
    fn record_output_failure(&self, now: Instant) {
        let mut health = lock_unpoisoned(&self.health);
        let previous = health.get(&TelemetryComponent::Telemetry).copied();
        let retry_count = previous.map_or(1, |record| record.retry_count.saturating_add(1));
        let stale_age_micros =
            previous
                .and_then(|record| record.last_success)
                .map(|last_success| {
                    now.saturating_duration_since(last_success)
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64
                });
        let event = ComponentHealthEvent {
            component: TelemetryComponent::Telemetry,
            state: ComponentHealthState::Failed,
            first_seen: now,
            last_changed: now,
            last_success: None,
            retry_count,
            stale_age_micros,
            persistence_gap: false,
            safe_reason: Some("telemetry output unavailable"),
        };
        let event = if let Some(previous) = previous {
            previous.normalize(event)
        } else {
            event
        };
        health.insert(
            TelemetryComponent::Telemetry,
            TelemetryHealthRecord::from_event(&event),
        );
    }

    /// 在故障后的完整 flush 成功时恢复进程内 Telemetry health。
    fn record_output_recovery(&self, now: Instant) {
        let mut health = lock_unpoisoned(&self.health);
        let Some(previous) = health.get(&TelemetryComponent::Telemetry).copied() else {
            return;
        };
        if previous.state != ComponentHealthState::Failed {
            return;
        }
        let event = previous.normalize(ComponentHealthEvent {
            component: TelemetryComponent::Telemetry,
            state: ComponentHealthState::Healthy,
            first_seen: now,
            last_changed: now,
            last_success: Some(now),
            retry_count: previous.retry_count,
            stale_age_micros: None,
            persistence_gap: false,
            safe_reason: None,
        });
        health.insert(
            TelemetryComponent::Telemetry,
            TelemetryHealthRecord::from_event(&event),
        );
    }
}

impl TelemetryItem {
    fn is_low_priority(&self) -> bool {
        matches!(
            self,
            Self::Log(LogEvent {
                level: TelemetryLogLevel::Trace
                    | TelemetryLogLevel::Debug
                    | TelemetryLogLevel::Info,
                ..
            })
        )
    }
}

impl LogSink for TelemetryWriter {
    fn emit(&self, event: LogEvent) -> Result<(), PortError> {
        let low_priority = matches!(
            event.level,
            TelemetryLogLevel::Trace | TelemetryLogLevel::Debug | TelemetryLogLevel::Info
        );
        self.enqueue(TelemetryItem::Log(event), low_priority)
    }

    fn flush(
        &self,
        deadline: Deadline,
    ) -> PortFuture<'_, Result<TelemetryFlushSummary, PortError>> {
        Box::pin(async move { self.flush_now(deadline) })
    }
}

impl MetricsSink for TelemetryWriter {
    fn record(&self, event: MetricEvent) -> Result<(), PortError> {
        event.validate().map_err(|_| {
            PortError::new(
                PortErrorClass::InvalidInput,
                "observability.telemetry.metric.validate",
            )
        })?;
        self.enqueue(TelemetryItem::Metric(event), false)
    }
}

impl HealthSink for TelemetryWriter {
    fn update(&self, event: ComponentHealthEvent) -> Result<(), PortError> {
        let mut health = lock_unpoisoned(&self.health);
        let event = health
            .get(&event.component)
            .map_or(event.clone(), |record| record.normalize(event));
        self.enqueue(TelemetryItem::Health(event.clone()), false)?;
        health.insert(event.component, TelemetryHealthRecord::from_event(&event));
        Ok(())
    }
}

pub const DEFAULT_TELEMETRY_QUEUE_CAPACITY: usize = 1_024;

#[derive(Debug, thiserror::Error)]
pub enum TelemetryRuntimeBuildError {
    #[error("bootstrap output is not initialized")]
    BootstrapNotInitialized,
    #[error("telemetry writer could not be created: {0}")]
    Writer(#[from] TelemetryWriterBuildError),
    #[error("final tracing layer could not be installed")]
    FinalLayerReload,
}

/// 创建与进程级 tracing 共用输出目标的运行时 telemetry writer。
pub fn build_runtime_telemetry() -> Result<Arc<TelemetryWriter>, TelemetryRuntimeBuildError> {
    let output = BOOTSTRAP_OUTPUT
        .get()
        .cloned()
        .ok_or(TelemetryRuntimeBuildError::BootstrapNotInitialized)?;
    let output = Arc::new(StructuredTelemetryOutput::shared(output));
    TelemetryWriter::new(DEFAULT_TELEMETRY_QUEUE_CAPACITY, output)
        .map(Arc::new)
        .map_err(TelemetryRuntimeBuildError::Writer)
}

struct TypedTracingLayer {
    writer: Arc<TelemetryWriter>,
}

impl<S> Layer<S> for TypedTracingLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut fields = TracingEventFields::default();
        event.record(&mut fields);
        let event_name = fields
            .event_name
            .as_deref()
            .and_then(|value| TelemetryEventName::parse(value.to_owned()).ok())
            .unwrap_or_else(|| {
                TelemetryEventName::parse("tracing.event").expect("static event name")
            });
        let log = LogEvent {
            occurred_at: std::time::SystemTime::now(),
            level: telemetry_level(*event.metadata().level()),
            name: event_name,
            component: fields
                .component
                .as_deref()
                .map(telemetry_component)
                .unwrap_or(TelemetryComponent::Application),
            request_digest: None,
            configured_id: None,
            outcome: fields
                .outcome
                .as_deref()
                .map(telemetry_outcome)
                .unwrap_or(OutcomeClass::Success),
            runtime_revision: fields.runtime_revision.map(crate::dns::RuntimeRevision),
            message: event.metadata().name(),
        };
        let _ = LogSink::emit(self.writer.as_ref(), log);
    }
}

#[derive(Default)]
struct TracingEventFields {
    event_name: Option<String>,
    component: Option<String>,
    outcome: Option<String>,
    runtime_revision: Option<u64>,
}

impl tracing::field::Visit for TracingEventFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "event" | "event_name" => self.event_name = Some(value.to_owned()),
            "component" => self.component = Some(value.to_owned()),
            "result" | "outcome" => self.outcome = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if matches!(field.name(), "revision" | "runtime_revision") {
            self.runtime_revision = Some(value);
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if value >= 0 && matches!(field.name(), "revision" | "runtime_revision") {
            self.runtime_revision = Some(value as u64);
        }
    }

    fn record_bool(&mut self, _field: &tracing::field::Field, _value: bool) {}

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn fmt::Debug) {}
}

fn telemetry_level(level: tracing::Level) -> TelemetryLogLevel {
    match level {
        tracing::Level::TRACE => TelemetryLogLevel::Trace,
        tracing::Level::DEBUG => TelemetryLogLevel::Debug,
        tracing::Level::INFO => TelemetryLogLevel::Info,
        tracing::Level::WARN => TelemetryLogLevel::Warn,
        tracing::Level::ERROR => TelemetryLogLevel::Error,
    }
}

fn telemetry_component(value: &str) -> TelemetryComponent {
    match value {
        "runtime" => TelemetryComponent::Runtime,
        "listener" => TelemetryComponent::Listener,
        "dns" => TelemetryComponent::Dns,
        "policy" => TelemetryComponent::Policy,
        "upstream" => TelemetryComponent::Upstream,
        "cache" => TelemetryComponent::Cache,
        "resource" => TelemetryComponent::Resource,
        "storage" => TelemetryComponent::Storage,
        "telemetry" => TelemetryComponent::Telemetry,
        _ => TelemetryComponent::Application,
    }
}

fn telemetry_outcome(value: &str) -> OutcomeClass {
    match value {
        "failure" | "failed" => OutcomeClass::Failure,
        "timeout" => OutcomeClass::Timeout,
        "cancelled" | "canceled" => OutcomeClass::Cancelled,
        "rejected" => OutcomeClass::Rejected,
        "dropped" => OutcomeClass::Dropped,
        _ => OutcomeClass::Success,
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
        fs,
        io::{self, Write},
        str::FromStr,
        sync::{Arc, Mutex as StdMutex},
        thread,
        time::{Duration, Instant, SystemTime},
    };

    use crate::dns::Deadline;
    use crate::ports::PortErrorClass;
    use crate::ports::telemetry::{
        Component as TelemetryComponent, ComponentHealthEvent, ComponentHealthState,
        EventName as TelemetryEventName, HealthSink, LogEvent as TelemetryLogEvent,
        LogLevel as TelemetryLogLevel, LogSink, MetricEvent as TelemetryMetricEvent,
        MetricName as TelemetryMetricName, MetricValue as TelemetryMetricValue, MetricsSink,
    };
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::{
        BufferedEvent, Component, EmitResult, EventName, EventResult, EventSink, EventSinkError,
        EventWriter, EventWriterError, HealthState, LogLevel, MetricKey, MetricName, MetricValue,
        ObservabilityRegistry, RegistryError, Sensitive, StructuredTelemetryOutput,
        TelemetryOutput, TelemetryWriter, TelemetryWriterBuildError, TypedEvent, TypedTracingLayer,
        bootstrap_subscriber, lock_unpoisoned,
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
    fn typed_tracing_layer_maps_safe_fields_into_telemetry_writer() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = Arc::new(TelemetryWriter::new(4, output.clone()).unwrap());
        let subscriber = tracing_subscriber::registry().with(TypedTracingLayer {
            writer: Arc::clone(&writer),
        });
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                event = "runtime_ready",
                component = "runtime",
                result = "success",
                revision = 7_u64,
                "runtime_ready"
            );
        });

        writer
            .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        let logs = output.logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0], "runtime_ready");
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

    #[derive(Default)]
    struct RecordingTelemetryOutput {
        fail: std::sync::atomic::AtomicBool,
        logs: StdMutex<Vec<String>>,
        metrics: StdMutex<usize>,
        health: StdMutex<usize>,
        health_events: StdMutex<Vec<ComponentHealthEvent>>,
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected output failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("injected output failure"))
        }
    }

    struct RecordingWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl RecordingTelemetryOutput {
        fn check(&self, operation: &'static str) -> Result<(), crate::ports::PortError> {
            if self.fail.load(std::sync::atomic::Ordering::Acquire) {
                Err(crate::ports::PortError::new(
                    PortErrorClass::Unavailable,
                    operation,
                ))
            } else {
                Ok(())
            }
        }
    }

    impl TelemetryOutput for RecordingTelemetryOutput {
        fn write_log(&self, event: &TelemetryLogEvent) -> Result<(), crate::ports::PortError> {
            self.check("test.telemetry.log")?;
            self.logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.name.as_str().to_owned());
            Ok(())
        }

        fn write_metric(
            &self,
            _event: &TelemetryMetricEvent,
        ) -> Result<(), crate::ports::PortError> {
            self.check("test.telemetry.metric")?;
            *self
                .metrics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            Ok(())
        }

        fn write_health(
            &self,
            event: &ComponentHealthEvent,
        ) -> Result<(), crate::ports::PortError> {
            self.check("test.telemetry.health")?;
            *self
                .health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            self.health_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.clone());
            Ok(())
        }
    }

    fn telemetry_log(level: TelemetryLogLevel) -> TelemetryLogEvent {
        TelemetryLogEvent {
            occurred_at: SystemTime::UNIX_EPOCH,
            level,
            name: TelemetryEventName::parse("dns.request.complete").unwrap(),
            component: TelemetryComponent::Dns,
            request_digest: None,
            configured_id: None,
            outcome: crate::ports::telemetry::OutcomeClass::Success,
            runtime_revision: None,
            message: "safe message",
        }
    }

    fn telemetry_health() -> ComponentHealthEvent {
        let now = Instant::now();
        ComponentHealthEvent {
            component: TelemetryComponent::Telemetry,
            state: ComponentHealthState::Healthy,
            first_seen: now,
            last_changed: now,
            last_success: Some(now),
            retry_count: 0,
            stale_age_micros: None,
            persistence_gap: false,
            safe_reason: None,
        }
    }

    #[test]
    fn telemetry_writer_is_bounded_and_preserves_high_priority_logs() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        assert!(matches!(
            TelemetryWriter::new(0, output.clone()),
            Err(TelemetryWriterBuildError::ZeroCapacity)
        ));
        let writer = TelemetryWriter::new(1, output.clone()).unwrap();

        LogSink::emit(&writer, telemetry_log(TelemetryLogLevel::Info)).unwrap();
        LogSink::emit(&writer, telemetry_log(TelemetryLogLevel::Debug)).unwrap();
        assert_eq!(writer.stats().dropped_low_priority(), 1);
        LogSink::emit(&writer, telemetry_log(TelemetryLogLevel::Error)).unwrap();
        assert_eq!(writer.stats().dropped_low_priority(), 2);

        let summary = writer
            .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(summary.emitted, 1);
        let logs = output.logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0], "dns.request.complete");
        assert_eq!(writer.stats().pending(), 0);
    }

    #[test]
    fn telemetry_writer_rejects_high_priority_when_no_lower_priority_can_be_evicted() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = TelemetryWriter::new(1, output).unwrap();
        LogSink::emit(&writer, telemetry_log(TelemetryLogLevel::Warn)).unwrap();
        let error = LogSink::emit(&writer, telemetry_log(TelemetryLogLevel::Error)).unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::ResourceExhausted));
        assert_eq!(writer.stats().pending(), 1);
    }

    #[test]
    fn telemetry_writer_queues_metrics_and_health_without_waiting_for_output() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = TelemetryWriter::new(2, output.clone()).unwrap();
        MetricsSink::record(
            &writer,
            TelemetryMetricEvent::new(
                TelemetryMetricName::RequestsTotal,
                Vec::new(),
                TelemetryMetricValue::Counter(1),
            )
            .unwrap(),
        )
        .unwrap();
        HealthSink::update(&writer, telemetry_health()).unwrap();
        writer
            .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(*output.metrics.lock().unwrap(), 1);
        assert_eq!(*output.health.lock().unwrap(), 1);
    }

    #[test]
    fn telemetry_writer_preserves_health_lifecycle_fields() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = TelemetryWriter::new(4, output.clone()).unwrap();
        let first = Instant::now();
        let changed = first + Duration::from_secs(1);
        let recovered = changed + Duration::from_secs(1);

        let mut failed = telemetry_health();
        failed.component = TelemetryComponent::Storage;
        failed.state = ComponentHealthState::Failed;
        failed.first_seen = first;
        failed.last_changed = first;
        failed.last_success = None;
        failed.stale_age_micros = Some(3_000_000);
        failed.persistence_gap = true;
        HealthSink::update(&writer, failed.clone()).unwrap();

        let mut repeated = failed.clone();
        repeated.first_seen = changed;
        repeated.last_changed = changed;
        repeated.retry_count = 2;
        HealthSink::update(&writer, repeated).unwrap();

        let mut healthy = failed;
        healthy.state = ComponentHealthState::Healthy;
        healthy.first_seen = recovered;
        healthy.last_changed = recovered;
        healthy.last_success = Some(recovered);
        healthy.retry_count = 0;
        healthy.stale_age_micros = Some(9_000_000);
        healthy.persistence_gap = false;
        HealthSink::update(&writer, healthy).unwrap();

        writer
            .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        let events = output
            .health_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].first_seen, first);
        assert_eq!(events[1].last_changed, first);
        assert_eq!(events[1].retry_count, 2);
        assert_eq!(events[1].stale_age_micros, Some(3_000_000));
        assert_eq!(events[2].first_seen, first);
        assert_eq!(events[2].last_changed, recovered);
        assert_eq!(events[2].last_success, Some(recovered));
        assert_eq!(events[2].retry_count, 2);
        assert_eq!(events[2].stale_age_micros, None);
        assert!(!events[2].persistence_gap);
    }

    #[tokio::test]
    async fn telemetry_writer_requeues_output_failures_and_honors_deadline() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = TelemetryWriter::new(1, output.clone()).unwrap();
        LogSink::emit(&writer, telemetry_log(TelemetryLogLevel::Error)).unwrap();
        output
            .fail
            .store(true, std::sync::atomic::Ordering::Release);
        let failed = writer.flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)));
        assert!(failed.is_err());
        assert_eq!(writer.stats().pending(), 1);
        assert_eq!(writer.stats().failed(), 1);
        {
            let health = lock_unpoisoned(&writer.health);
            let record = health.get(&TelemetryComponent::Telemetry).unwrap();
            assert_eq!(record.state, ComponentHealthState::Failed);
            assert_eq!(record.retry_count, 1);
        }

        let expired = writer.flush_now(Deadline::new(Instant::now()));
        assert!(matches!(
            expired.unwrap_err().class(),
            PortErrorClass::Timeout
        ));
        assert_eq!(writer.stats().pending(), 1);

        output
            .fail
            .store(false, std::sync::atomic::Ordering::Release);
        let summary = LogSink::flush(
            &writer,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert_eq!(summary.emitted, 1);
        assert_eq!(writer.stats().pending(), 0);
        {
            let health = lock_unpoisoned(&writer.health);
            let record = health.get(&TelemetryComponent::Telemetry).unwrap();
            assert_eq!(record.state, ComponentHealthState::Healthy);
            assert_eq!(record.retry_count, 1);
            assert!(record.last_success.is_some());
        }
    }

    #[test]
    fn telemetry_writer_shutdown_closes_future_emits_after_flush() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = TelemetryWriter::new(1, output).unwrap();
        LogSink::emit(&writer, telemetry_log(TelemetryLogLevel::Info)).unwrap();
        writer
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        assert!(writer.stats().closed());
        let error = LogSink::emit(&writer, telemetry_log(TelemetryLogLevel::Info)).unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::Unavailable));
    }

    #[test]
    fn structured_output_writes_typed_events_to_a_real_file() {
        let path = std::env::temp_dir().join(format!(
            "fluxdns-telemetry-output-{}-{}.log",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = StructuredTelemetryOutput::file(&path).unwrap();
        output
            .write_log(&telemetry_log(TelemetryLogLevel::Info))
            .unwrap();
        output
            .write_metric(
                &TelemetryMetricEvent::new(
                    TelemetryMetricName::RequestsTotal,
                    Vec::new(),
                    TelemetryMetricValue::Counter(1),
                )
                .unwrap(),
            )
            .unwrap();
        output.write_health(&telemetry_health()).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"kind\":\"log\""));
        assert!(content.contains("\"event\":\"dns.request.complete\""));
        assert!(content.contains("\"kind\":\"metric\""));
        assert!(content.contains("\"kind\":\"health\""));
        assert!(!content.contains("raw_dns_wire"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn structured_output_falls_back_when_primary_writer_fails() {
        let fallback = Arc::new(StdMutex::new(Vec::new()));
        let output = StructuredTelemetryOutput::from_writer_with_fallback(
            Box::new(FailingWriter),
            Some(Box::new(RecordingWriter(Arc::clone(&fallback)))),
        );

        output
            .write_log(&telemetry_log(TelemetryLogLevel::Error))
            .unwrap();

        let content = String::from_utf8(
            fallback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
        .unwrap();
        assert!(content.contains("\"kind\":\"log\""));
        assert!(content.contains("\"event\":\"dns.request.complete\""));
    }

    /// 验证主输出与 fallback 同时失败时返回稳定、安全的错误边界。
    #[test]
    fn structured_output_reports_failure_when_primary_and_fallback_fail() {
        let output = StructuredTelemetryOutput::from_writer_with_fallback(
            Box::new(FailingWriter),
            Some(Box::new(FailingWriter)),
        );

        let error = output
            .write_log(&telemetry_log(TelemetryLogLevel::Error))
            .unwrap_err();
        assert!(matches!(error.class(), PortErrorClass::Unavailable));
        assert_eq!(error.operation(), "observability.telemetry.output");
    }
}
