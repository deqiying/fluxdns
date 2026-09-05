use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{Instant, SystemTime},
};

use crate::dns::Deadline;
use crate::ports::telemetry::{
    Component as TelemetryComponent, ComponentHealthEvent, ComponentHealthState,
    EventName as TelemetryEventName, HealthSink, LogEvent, LogLevel as TelemetryLogLevel, LogSink,
    MetricEvent, MetricValue, MetricsSink, OutcomeClass, TelemetryFlushSummary,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};

mod registry;
pub use registry::MetricSnapshot;
use registry::{ObservabilityRegistry, RegistryError};

static NEXT_WRITER_INSTANCE: AtomicU64 = AtomicU64::new(0);

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
    rejected_metrics: u64,
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

    pub const fn rejected_metrics(self) -> u64 {
        self.rejected_metrics
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
    /// 输出累计/瞬时快照；重复输出不能作为新输入再次累加。
    fn write_metric_snapshot(
        &self,
        instance: &str,
        metric: &MetricSnapshot,
    ) -> Result<(), PortError>;
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
    /// 绑定同步输出目标；该入口不配置失败 fallback。
    pub(crate) fn from_writer(writer: Box<dyn Write + Send>) -> Self {
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

    fn write_metric_snapshot(
        &self,
        instance: &str,
        metric: &MetricSnapshot,
    ) -> Result<(), PortError> {
        let temporality = match metric.value {
            MetricValue::Counter(_) => "cumulative",
            MetricValue::Gauge(_) => "instantaneous",
            MetricValue::DurationMicros(_) => unreachable!("duration events are not aggregated"),
        };
        self.write_line(
            &serde_json::json!({
                "kind": "metric",
                "writer_instance": instance,
                "occurred_at_ms": system_time_millis(SystemTime::now()),
                "name": enum_name(metric.name),
                "labels": [{"key": "component", "value": enum_name(metric.component)}],
                "value": format!("{:?}", metric.value),
                "temporality": temporality,
            })
            .to_string(),
        )
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
    in_flight: usize,
    emitted: u64,
    dropped_low_priority: u64,
    failed: u64,
    closed: bool,
    rejected_metrics: u64,
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
    safe_reason: Option<&'static str>,
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
            safe_reason: event.safe_reason,
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

/// Management API 可读取的有界健康快照，不包含日志正文或任意请求字段。
#[derive(Clone, Copy, Debug)]
pub(crate) struct TelemetryHealthSnapshot {
    pub(crate) component: TelemetryComponent,
    pub(crate) state: ComponentHealthState,
    pub(crate) first_seen: Instant,
    pub(crate) last_changed: Instant,
    pub(crate) last_success: Option<Instant>,
    pub(crate) retry_count: u64,
    pub(crate) stale: bool,
    pub(crate) persistence_gap: bool,
    pub(crate) safe_reason: Option<&'static str>,
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
    metrics: ObservabilityRegistry,
    instance: String,
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
                in_flight: 0,
                emitted: 0,
                dropped_low_priority: 0,
                failed: 0,
                closed: false,
                rejected_metrics: 0,
            }),
            flush_lock: Mutex::new(()),
            health: Mutex::new(BTreeMap::new()),
            metrics: ObservabilityRegistry::new(),
            instance: format!(
                "{}-{}-{}",
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                std::process::id(),
                NEXT_WRITER_INSTANCE.fetch_add(1, Ordering::Relaxed)
            ),
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
            rejected_metrics: state.rejected_metrics,
        }
    }

    pub(crate) fn metric_snapshot(&self) -> Vec<MetricSnapshot> {
        self.metrics.snapshot()
    }

    /// 采样失败只更新固定诊断计数，不通过故障中的指标/日志路径递归发布。
    pub(crate) fn record_metric_rejection(&self) {
        let mut state = lock_unpoisoned(&self.state);
        state.rejected_metrics = state.rejected_metrics.saturating_add(1);
    }

    pub(crate) fn health_snapshot(&self) -> Vec<TelemetryHealthSnapshot> {
        let health = lock_unpoisoned(&self.health);
        health
            .iter()
            .map(|(component, record)| TelemetryHealthSnapshot {
                component: *component,
                state: record.state,
                first_seen: record.first_seen,
                last_changed: record.last_changed,
                last_success: record.last_success,
                retry_count: record.retry_count,
                stale: record.stale_age_micros.is_some(),
                persistence_gap: record.persistence_gap,
                safe_reason: record.safe_reason,
            })
            .collect()
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

        if state.queue.len() + state.in_flight < self.capacity {
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
        let batch_len = lock_unpoisoned(&self.state).queue.len();
        // 每次只处理有界批次，持续日志入队不能饿死聚合快照输出。
        self.check_flush_deadline(deadline)?;
        for _ in 0..batch_len {
            if deadline.is_expired(Instant::now()) {
                return Err(PortError::new(
                    PortErrorClass::Timeout,
                    "observability.telemetry.flush",
                ));
            }

            let item = {
                let mut state = lock_unpoisoned(&self.state);
                let item = state.queue.pop_front();
                state.in_flight += usize::from(item.is_some());
                item
            };
            let Some(item) = item else {
                break;
            };

            let result = match &item {
                TelemetryItem::Log(event) => self.output.write_log(event),
                TelemetryItem::Metric(event) => self.output.write_metric(event),
                TelemetryItem::Health(event) => self.output.write_health(event),
            };
            match result {
                Ok(()) => {
                    let mut state = lock_unpoisoned(&self.state);
                    state.in_flight -= 1;
                    state.emitted = state.emitted.saturating_add(1);
                }
                Err(error) => {
                    let mut state = lock_unpoisoned(&self.state);
                    state.in_flight -= 1;
                    state.failed = state.failed.saturating_add(1);
                    state.queue.push_front(item);
                    drop(state);
                    self.record_output_failure(Instant::now());
                    return Err(error);
                }
            }
        }
        for metric in self.metric_snapshot() {
            self.check_flush_deadline(deadline)?;
            match self.output.write_metric_snapshot(&self.instance, &metric) {
                Ok(()) => {
                    let mut state = lock_unpoisoned(&self.state);
                    state.emitted = state.emitted.saturating_add(1);
                }
                Err(error) => {
                    let mut state = lock_unpoisoned(&self.state);
                    state.failed = state.failed.saturating_add(1);
                    drop(state);
                    self.record_output_failure(Instant::now());
                    return Err(error);
                }
            }
        }
        self.check_flush_deadline(deadline)?;
        self.record_output_recovery(Instant::now());
        let state = lock_unpoisoned(&self.state);
        Ok(TelemetryFlushSummary {
            emitted: state.emitted,
            dropped_low_priority: state.dropped_low_priority,
            failed: state.failed,
        })
    }

    fn check_flush_deadline(&self, deadline: Deadline) -> Result<(), PortError> {
        if deadline.is_expired(Instant::now()) {
            return Err(PortError::new(
                PortErrorClass::Timeout,
                "observability.telemetry.flush",
            ));
        }
        Ok(())
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
        if matches!(event.value(), MetricValue::DurationMicros(_)) {
            if !matches!(
                event.name(),
                crate::ports::telemetry::MetricName::RequestLatency
                    | crate::ports::telemetry::MetricName::UpstreamLatency
            ) {
                self.record_metric_rejection();
                return Err(PortError::new(
                    PortErrorClass::InvalidInput,
                    "observability.telemetry.metric.validate",
                ));
            }
            let result = event
                .validate()
                .map_err(|_| {
                    PortError::new(
                        PortErrorClass::InvalidInput,
                        "observability.telemetry.metric.validate",
                    )
                })
                .and_then(|()| self.enqueue(TelemetryItem::Metric(event), false));
            if result.is_err() {
                self.record_metric_rejection();
            }
            return result;
        }
        // 与 shutdown 使用同一 lifecycle lock，接收成功的更新必须进入最终快照。
        let mut state = lock_unpoisoned(&self.state);
        let result = if state.closed {
            Err(PortError::new(
                PortErrorClass::Unavailable,
                "observability.telemetry.metric",
            )
            .with_safe_context("writer closed"))
        } else {
            self.metrics.record(&event).map_err(|error| {
                let class = match error {
                    RegistryError::MetricCapacityExhausted | RegistryError::CounterOverflow => {
                        PortErrorClass::ResourceExhausted
                    }
                    _ => PortErrorClass::InvalidInput,
                };
                PortError::new(class, "observability.telemetry.metric")
            })
        };
        if result.is_err() {
            state.rejected_metrics = state.rejected_metrics.saturating_add(1);
        }
        result
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
        "resolution" => TelemetryComponent::Resolution,
        "policy" => TelemetryComponent::Policy,
        "upstream" => TelemetryComponent::Upstream,
        "cache" => TelemetryComponent::Cache,
        "resource" => TelemetryComponent::Resource,
        "storage" => TelemetryComponent::Storage,
        "telemetry" => TelemetryComponent::Telemetry,
        "management" => TelemetryComponent::Management,
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
        LogLevel, MetricSnapshot, Sensitive, StructuredTelemetryOutput, TelemetryOutput,
        TelemetryWriter, TelemetryWriterBuildError, TypedTracingLayer, bootstrap_subscriber,
        lock_unpoisoned,
    };

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

    #[derive(Default)]
    struct RecordingTelemetryOutput {
        fail: std::sync::atomic::AtomicBool,
        logs: StdMutex<Vec<String>>,
        metrics: StdMutex<usize>,
        health: StdMutex<usize>,
        health_events: StdMutex<Vec<ComponentHealthEvent>>,
        snapshots: StdMutex<Vec<MetricSnapshot>>,
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
        fn write_metric_snapshot(
            &self,
            _instance: &str,
            metric: &MetricSnapshot,
        ) -> Result<(), crate::ports::PortError> {
            self.check("test.telemetry.metric_snapshot")?;
            self.snapshots.lock().unwrap().push(*metric);
            Ok(())
        }

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

    fn accepted_metric(value: u64) -> TelemetryMetricEvent {
        use crate::ports::telemetry::{MetricLabel, MetricLabelKey, MetricLabelValue};
        TelemetryMetricEvent::new(
            TelemetryMetricName::ResolutionEventsAccepted,
            vec![
                MetricLabel::new(
                    MetricLabelKey::Component,
                    MetricLabelValue::Component(TelemetryComponent::Resolution),
                )
                .unwrap(),
            ],
            TelemetryMetricValue::Counter(value),
        )
        .unwrap()
    }

    #[test]
    fn aggregated_metrics_survive_log_congestion_and_output_retry() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = TelemetryWriter::new(1, output.clone()).unwrap();
        writer.emit(telemetry_log(TelemetryLogLevel::Warn)).unwrap();
        writer.record(accepted_metric(3)).unwrap();
        writer.record(accepted_metric(4)).unwrap();
        assert_eq!(writer.stats().pending(), 1);
        assert_eq!(
            writer.metric_snapshot()[0].value,
            TelemetryMetricValue::Counter(7)
        );
        output.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            writer
                .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
                .is_err()
        );
        assert_eq!(writer.stats().pending(), 1);
        assert_eq!(
            writer.metric_snapshot()[0].value,
            TelemetryMetricValue::Counter(7)
        );
        output
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        writer
            .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        writer
            .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            output
                .snapshots
                .lock()
                .unwrap()
                .iter()
                .map(|item| item.value)
                .collect::<Vec<_>>(),
            vec![TelemetryMetricValue::Counter(7); 2]
        );
        // 快照写失败不制造待发队列，也不丢掉已聚合的值。
        output.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            writer
                .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
                .is_err()
        );
        writer.record(accepted_metric(2)).unwrap();
        assert_eq!(writer.stats().pending(), 0);
        output
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        writer
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            output.snapshots.lock().unwrap().last().unwrap().value,
            TelemetryMetricValue::Counter(9)
        );
        assert!(writer.record(accepted_metric(1)).is_err());
        assert_eq!(writer.stats().rejected_metrics(), 1);
    }

    #[test]
    fn shutdown_snapshot_contains_every_accepted_concurrent_metric() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = Arc::new(TelemetryWriter::new(1, output.clone()).unwrap());
        writer.record(accepted_metric(0)).unwrap();
        let accepted = Arc::new(AtomicU64::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let writer = writer.clone();
                let accepted = accepted.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..100 {
                        if writer.record(accepted_metric(1)).is_ok() {
                            accepted.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        barrier.wait();
        writer
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(2)))
            .unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            output.snapshots.lock().unwrap().last().unwrap().value,
            TelemetryMetricValue::Counter(accepted.load(Ordering::SeqCst))
        );
    }

    #[test]
    fn aggregated_output_declares_temporality_and_writer_identity() {
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let output = Arc::new(StructuredTelemetryOutput::from_writer(Box::new(
            RecordingWriter(bytes.clone()),
        )));
        let writer = TelemetryWriter::new(1, output.clone()).unwrap();
        writer.record(accepted_metric(2)).unwrap();
        writer
            .flush_now(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        writer
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        let next = TelemetryWriter::new(1, output).unwrap();
        next.record(accepted_metric(1)).unwrap();
        next.shutdown(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let rows: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["temporality"], "cumulative");
        assert_eq!(rows[0]["value"], "Counter(2)");
        assert_eq!(rows[0]["writer_instance"], rows[1]["writer_instance"]);
        assert_ne!(rows[0]["writer_instance"], rows[2]["writer_instance"]);
        assert!(!text.contains("request_digest"));
    }

    #[test]
    fn unknown_metric_kinds_are_rejected_without_mutation() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = TelemetryWriter::new(1, output).unwrap();
        writer.record(accepted_metric(u64::MAX)).unwrap();
        assert!(writer.record(accepted_metric(1)).is_err());
        assert!(
            writer
                .record(
                    TelemetryMetricEvent::new(
                        TelemetryMetricName::ResolutionEventsAccepted,
                        vec![],
                        TelemetryMetricValue::DurationMicros(1),
                    )
                    .unwrap()
                )
                .is_err()
        );
        assert_eq!(writer.stats().rejected_metrics(), 2);
        assert_eq!(
            writer.metric_snapshot()[0].value,
            TelemetryMetricValue::Counter(u64::MAX)
        );
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
    fn telemetry_writer_queues_duration_samples_and_health_without_waiting_for_output() {
        let output = Arc::new(RecordingTelemetryOutput::default());
        let writer = TelemetryWriter::new(2, output.clone()).unwrap();
        MetricsSink::record(
            &writer,
            TelemetryMetricEvent::new(
                TelemetryMetricName::RequestLatency,
                Vec::new(),
                TelemetryMetricValue::DurationMicros(1),
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
