//! Management 服务指标的进程级采集 owner。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::contract::{
    DecimalU64, HostEnvironment, MAX_ONLINE_IDENTITIES, Measurement, ProcessMetrics, RateSample,
    ServiceMetrics, ServiceMetricsDelta, UnavailableReason,
};
use crate::dns::{Cancellation, ClientIdentity};
use crate::runtime::TaskError;

const REQUEST_WINDOW_SECONDS: u64 = 600;
const QPS_WINDOW_SECONDS: u64 = 60;
const RPM_WINDOW_SECONDS: u64 = 600;
/// 逐秒趋势比请求窗口多带一个 60 秒前瞻：管理面按逐秒样本滚动求和推导 RPM，
/// 十分钟视图的最左一分钟需要窗口起点之前的 60 个秒桶，否则该分钟只能报不可用。
const QPS_TREND_SECONDS: u64 = REQUEST_WINDOW_SECONDS + QPS_WINDOW_SECONDS;
/// 环形桶数比最长保留跨度多 1，保证任一需要输出的秒桶都不会被同槽位覆盖。
const REQUEST_BUCKET_COUNT: usize = QPS_TREND_SECONDS as usize + 1;
/// 分钟趋势与增量帧最多覆盖的已完成分钟数。
const RPM_TREND_MINUTES: u64 = 10;
const ONLINE_WINDOW_SECONDS: u64 = 60;
const PROCESS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const PROCESS_SAMPLE_STALE_AFTER: Duration = Duration::from_secs(3);
/// 主机文本字段的契约上限；超长值截断，避免响应超出 schema 的 maxLength。
const HOST_TEXT_MAX_CHARS: usize = 128;
const HOSTNAME_MAX_CHARS: usize = 255;
/// 契约里逻辑核心数的上限；实际并行度不会达到，仅用于保证响应始终合法。
const LOGICAL_CORES_MAX: u32 = 1024;

/// 增量推送游标：该订阅已经收到并把基线推进到的下一批秒桶/分钟桶序号（`elapsed` 秒口径）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MetricsCursor {
    pub(crate) next_second: u64,
    pub(crate) next_minute: u64,
}

impl MetricsCursor {
    fn at(elapsed: u64) -> Self {
        Self {
            next_second: elapsed,
            next_minute: elapsed / 60,
        }
    }
}

/// 三张标量卡片的当前值；快照与增量帧共用，避免两条路径对窗口口径产生偏差。
#[derive(Clone, Debug)]
struct RequestScalars {
    qps: Measurement<f64>,
    rpm: Measurement<f64>,
    online_clients: Measurement<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RequestBucket {
    second: u64,
    count: u64,
    occupied: bool,
}

struct RequestState {
    buckets: Box<[RequestBucket; REQUEST_BUCKET_COUNT]>,
    online: HashMap<[u8; 32], u64>,
    online_gap_until: Option<u64>,
}

impl Default for RequestState {
    fn default() -> Self {
        Self {
            buckets: Box::new([RequestBucket::default(); REQUEST_BUCKET_COUNT]),
            online: HashMap::with_capacity(MAX_ONLINE_IDENTITIES),
            online_gap_until: None,
        }
    }
}

#[allow(dead_code)] // Unsupported 仅在非 Windows/Linux 目标构造。
#[derive(Clone, Copy, Debug, PartialEq)]
enum SampleValue<T> {
    Available(T),
    Warmup,
    Failed,
    Unsupported,
}

#[derive(Clone, Copy, Debug)]
struct ProcessSnapshot {
    sampled_at_ms: u64,
    sampled_instant: Instant,
    rss_bytes: SampleValue<u64>,
    cpu_percent: SampleValue<f64>,
    threads: SampleValue<u32>,
}

impl ProcessSnapshot {
    fn warming(sampled_at_ms: u64, sampled_instant: Instant) -> Self {
        Self {
            sampled_at_ms,
            sampled_instant,
            rss_bytes: SampleValue::Warmup,
            cpu_percent: SampleValue::Warmup,
            threads: SampleValue::Warmup,
        }
    }
}

/// 请求热路径和 OS 采样任务共享的唯一指标 owner。
pub(crate) struct MetricsOwner {
    started_at: SystemTime,
    started_instant: Instant,
    host: HostEnvironment,
    requests: Mutex<RequestState>,
    process: RwLock<ProcessSnapshot>,
}

impl MetricsOwner {
    pub(crate) fn new() -> Self {
        let started_at = SystemTime::now();
        Self::new_at(started_at, Instant::now())
    }

    fn new_at(started_at: SystemTime, started_instant: Instant) -> Self {
        Self {
            started_at,
            started_instant,
            // 主机信息在进程生命周期内不变，只在 owner 构造时读一次，避免请求路径重复触碰 OS API。
            host: host_environment(),
            requests: Mutex::new(RequestState::default()),
            process: RwLock::new(ProcessSnapshot::warming(
                unix_millis(started_at),
                started_instant,
            )),
        }
    }

    /// 在 transport 已完成基础校验且 Runtime 接纳请求后计数；不保存原始身份。
    pub(crate) fn record_request(&self, client: &ClientIdentity) {
        self.record_request_at(Instant::now(), client);
    }

    fn record_request_at(&self, now: Instant, client: &ClientIdentity) {
        let second = self.elapsed_seconds(now);
        let identity = online_identity(client);
        let mut state = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bucket = &mut state.buckets[second as usize % REQUEST_BUCKET_COUNT];
        if !bucket.occupied || bucket.second != second {
            *bucket = RequestBucket {
                second,
                count: 0,
                occupied: true,
            };
        }
        bucket.count = bucket.count.saturating_add(1);

        state
            .online
            .retain(|_, last_seen| second.saturating_sub(*last_seen) < ONLINE_WINDOW_SECONDS);
        let Some(identity) = identity else {
            return;
        };
        if let Some(last_seen) = state.online.get_mut(&identity) {
            *last_seen = second;
        } else if state.online.len() < MAX_ONLINE_IDENTITIES {
            state.online.insert(identity, second);
        } else {
            state.online_gap_until = Some(second.saturating_add(ONLINE_WINDOW_SECONDS));
        }
    }

    pub(crate) fn service_metrics(&self) -> ServiceMetrics {
        self.service_metrics_at(SystemTime::now(), Instant::now())
    }

    /// 订阅基线：全量快照与随它一起生效的增量游标必须在同一把锁内取，避免首帧重复或漏秒。
    pub(crate) fn service_metrics_with_cursor(&self) -> (ServiceMetrics, MetricsCursor) {
        self.service_metrics_with_cursor_at(SystemTime::now(), Instant::now())
    }

    pub(crate) fn service_metrics_delta(
        &self,
        cursor: MetricsCursor,
    ) -> Option<(ServiceMetricsDelta, MetricsCursor)> {
        self.service_metrics_delta_at(cursor, SystemTime::now(), Instant::now())
    }

    fn service_metrics_at(&self, sampled_at: SystemTime, now: Instant) -> ServiceMetrics {
        self.service_metrics_with_cursor_at(sampled_at, now).0
    }

    fn service_metrics_with_cursor_at(
        &self,
        sampled_at: SystemTime,
        now: Instant,
    ) -> (ServiceMetrics, MetricsCursor) {
        let mut state = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 秒序必须在持锁之后确定：若先读时钟再等锁，期间跨整秒的请求会写进
        // `elapsed - QPS_TREND_SECONDS` 的同槽位，使即将输出的最旧前瞻秒桶被读成 0。
        // `max` 只是让测试注入的确定性时钟继续生效，生产路径取到的是持锁后的实时钟。
        let elapsed = self.elapsed_seconds(now.max(Instant::now()));
        let scalars = request_scalars(&mut state, elapsed);
        let process = *self
            .process
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let completed_minutes = elapsed / 60;
        let metrics = ServiceMetrics {
            sampled_at_ms: unix_millis(sampled_at),
            qps: scalars.qps,
            rpm: scalars.rpm,
            online_clients: scalars.online_clients,
            rss_bytes: map_process_bytes(process, now, elapsed),
            cpu_percent: map_process_measurement(
                process.cpu_percent,
                process.sampled_instant,
                now,
                elapsed,
            ),
            qps_trend: second_samples(
                &state,
                self.started_at,
                elapsed.saturating_sub(elapsed.min(QPS_TREND_SECONDS)),
                elapsed,
            ),
            rpm_trend: minute_samples(
                &state,
                self.started_at,
                completed_minutes.saturating_sub(completed_minutes.min(RPM_TREND_MINUTES)),
                completed_minutes,
            ),
        };
        (metrics, MetricsCursor::at(elapsed))
    }

    /// 只带新增秒桶/分钟桶的后缀帧。返回 `None` 表示订阅游标已落后于环形缓冲可补范围，
    /// 调用方必须重发全量快照重新建立基线，而不是发送带洞的增量。
    fn service_metrics_delta_at(
        &self,
        cursor: MetricsCursor,
        sampled_at: SystemTime,
        now: Instant,
    ) -> Option<(ServiceMetricsDelta, MetricsCursor)> {
        let mut state = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 与快照路径同理：持锁后再确定秒序，避免最旧可补秒桶被同槽位的新秒覆盖。
        let elapsed = self.elapsed_seconds(now.max(Instant::now()));
        let next_second = cursor.next_second.min(elapsed);
        let completed_minutes = elapsed / 60;
        let next_minute = cursor.next_minute.min(completed_minutes);
        if next_second < elapsed.saturating_sub(QPS_TREND_SECONDS)
            || next_minute < completed_minutes.saturating_sub(RPM_TREND_MINUTES)
        {
            return None;
        }
        let scalars = request_scalars(&mut state, elapsed);
        let process = *self
            .process
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let delta = ServiceMetricsDelta {
            sampled_at_ms: unix_millis(sampled_at),
            qps: scalars.qps,
            rpm: scalars.rpm,
            online_clients: scalars.online_clients,
            rss_bytes: map_process_bytes(process, now, elapsed),
            cpu_percent: map_process_measurement(
                process.cpu_percent,
                process.sampled_instant,
                now,
                elapsed,
            ),
            qps_samples: second_samples(&state, self.started_at, next_second, elapsed),
            rpm_samples: minute_samples(&state, self.started_at, next_minute, completed_minutes),
        };
        Some((delta, MetricsCursor::at(elapsed)))
    }

    pub(crate) fn process_metrics(&self) -> ProcessMetrics {
        self.process_metrics_at(Instant::now())
    }

    fn process_metrics_at(&self, now: Instant) -> ProcessMetrics {
        let snapshot = *self
            .process
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let uptime_seconds = self.elapsed_seconds(now);
        ProcessMetrics {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at_ms: unix_millis(self.started_at),
            sampled_at_ms: snapshot.sampled_at_ms,
            uptime_seconds,
            rss_bytes: map_process_bytes(snapshot, now, uptime_seconds),
            cpu_percent: map_process_measurement(
                snapshot.cpu_percent,
                snapshot.sampled_instant,
                now,
                uptime_seconds,
            ),
            threads: map_process_measurement(
                snapshot.threads,
                snapshot.sampled_instant,
                now,
                uptime_seconds,
            ),
            host: self.host.clone(),
        }
    }

    /// 单一后台任务周期更新 OS 快照；采样失败只降级字段，不终止服务。
    pub(crate) async fn run_process_sampler(
        self: std::sync::Arc<Self>,
        cancellation: Cancellation,
    ) -> Result<(), TaskError> {
        let mut sampler = platform::ProcessSampler::default();
        loop {
            self.publish_process_sample(SystemTime::now(), Instant::now(), sampler.sample());
            tokio::select! {
                _ = cancellation.cancelled() => return Err(TaskError::Cancelled),
                _ = tokio::time::sleep(PROCESS_SAMPLE_INTERVAL) => {}
            }
        }
    }

    fn publish_process_sample(
        &self,
        sampled_at: SystemTime,
        sampled_instant: Instant,
        sample: ProcessSample,
    ) {
        *self
            .process
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ProcessSnapshot {
            sampled_at_ms: unix_millis(sampled_at),
            sampled_instant,
            rss_bytes: sample.rss_bytes,
            cpu_percent: sample.cpu_percent,
            threads: sample.threads,
        };
    }

    fn elapsed_seconds(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started_instant)
            .as_secs()
    }

    #[cfg(test)]
    pub(crate) fn accepted_requests_for_test(&self) -> u64 {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .buckets
            .iter()
            .map(|bucket| bucket.count)
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn set_process_sample_for_test(
        &self,
        rss_bytes: u64,
        cpu_percent: f64,
        threads: u32,
    ) {
        self.publish_process_sample(
            SystemTime::now(),
            Instant::now(),
            ProcessSample {
                rss_bytes: SampleValue::Available(rss_bytes),
                cpu_percent: SampleValue::Available(cpu_percent),
                threads: SampleValue::Available(threads),
            },
        );
    }
}

#[derive(Clone, Copy, Debug)]
struct ProcessSample {
    rss_bytes: SampleValue<u64>,
    cpu_percent: SampleValue<f64>,
    threads: SampleValue<u32>,
}

fn count_window(state: &RequestState, now: u64, window_seconds: u64) -> u64 {
    let first = now.saturating_sub(window_seconds.saturating_sub(1));
    state
        .buckets
        .iter()
        .filter(|bucket| bucket.occupied && bucket.second >= first && bucket.second <= now)
        .fold(0_u64, |total, bucket| total.saturating_add(bucket.count))
}

fn window_measurement(elapsed: u64, window: u64, value: f64) -> Measurement<f64> {
    if elapsed < window {
        unavailable(UnavailableReason::Warmup, Some(elapsed))
    } else {
        Measurement::Available { value }
    }
}

/// 三张标量卡片与两条趋势共用同一把锁内的秒桶视图；在线身份同时按 60 秒窗口清理。
fn request_scalars(state: &mut RequestState, elapsed: u64) -> RequestScalars {
    state
        .online
        .retain(|_, last_seen| elapsed.saturating_sub(*last_seen) < ONLINE_WINDOW_SECONDS);
    let qps = window_measurement(
        elapsed,
        QPS_WINDOW_SECONDS,
        count_window(state, elapsed, QPS_WINDOW_SECONDS) as f64 / QPS_WINDOW_SECONDS as f64,
    );
    let rpm = window_measurement(
        elapsed,
        RPM_WINDOW_SECONDS,
        count_window(state, elapsed, RPM_WINDOW_SECONDS) as f64 / 10.0,
    );
    let online_clients = if elapsed < ONLINE_WINDOW_SECONDS {
        unavailable(UnavailableReason::Warmup, Some(elapsed))
    } else if state
        .online_gap_until
        .is_some_and(|gap_until| elapsed < gap_until)
    {
        unavailable(UnavailableReason::ObservationGap, None)
    } else {
        Measurement::Available {
            value: u32::try_from(state.online.len()).unwrap_or(u32::MAX),
        }
    };
    RequestScalars {
        qps,
        rpm,
        online_clients,
    }
}

/// 逐秒桶样本：覆盖秒序 `[first, last)`；快照给完整窗口，增量帧只给新增后缀。
fn second_samples(
    state: &RequestState,
    started_at: SystemTime,
    first: u64,
    last: u64,
) -> Vec<RateSample> {
    (first..last)
        .map(|second| RateSample {
            at_ms: unix_millis(started_at + Duration::from_secs(second)),
            value: Measurement::Available {
                value: count_at(state, second) as f64,
            },
        })
        .collect()
}

/// 分钟桶样本：覆盖分钟序 `[first, last)`，每分钟为该分钟 60 个秒桶之和。
fn minute_samples(
    state: &RequestState,
    started_at: SystemTime,
    first: u64,
    last: u64,
) -> Vec<RateSample> {
    (first..last)
        .map(|minute| {
            let first_second = minute * 60;
            let count = (first_second..first_second + 60).fold(0_u64, |total, second| {
                total.saturating_add(count_at(state, second))
            });
            RateSample {
                at_ms: unix_millis(started_at + Duration::from_secs(first_second)),
                value: Measurement::Available {
                    value: count as f64,
                },
            }
        })
        .collect()
}

fn count_at(state: &RequestState, second: u64) -> u64 {
    let bucket = state.buckets[second as usize % REQUEST_BUCKET_COUNT];
    if bucket.occupied && bucket.second == second {
        bucket.count
    } else {
        0
    }
}

fn online_identity(client: &ClientIdentity) -> Option<[u8; 32]> {
    let mut hasher = Sha256::new();
    if let Some(client_id) = &client.client_id {
        hasher.update(b"client-id\0");
        hasher.update(client_id.as_str().as_bytes());
    } else {
        let address = normalize_ip(client.client_addr?);
        match address {
            IpAddr::V4(address) => {
                hasher.update(b"client-ipv4\0");
                hasher.update(address.octets());
            }
            IpAddr::V6(address) => {
                hasher.update(b"client-ipv6\0");
                hasher.update(address.octets());
            }
        }
    }
    Some(hasher.finalize().into())
}

fn normalize_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        IpAddr::V4(_) => address,
    }
}

fn map_process_bytes(
    snapshot: ProcessSnapshot,
    now: Instant,
    observed_seconds: u64,
) -> Measurement<DecimalU64> {
    map_process_measurement(
        snapshot.rss_bytes,
        snapshot.sampled_instant,
        now,
        observed_seconds,
    )
    .map(DecimalU64::from)
}

trait MapMeasurement<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> Measurement<U>;
}

impl<T> MapMeasurement<T> for Measurement<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> Measurement<U> {
        match self {
            Measurement::Available { value } => Measurement::Available { value: map(value) },
            Measurement::Unavailable {
                reason,
                observed_seconds,
            } => Measurement::Unavailable {
                reason,
                observed_seconds,
            },
        }
    }
}

fn map_measurement<T>(value: SampleValue<T>, observed_seconds: u64) -> Measurement<T> {
    match value {
        SampleValue::Available(value) => Measurement::Available { value },
        SampleValue::Warmup => unavailable(UnavailableReason::Warmup, Some(observed_seconds)),
        SampleValue::Failed => unavailable(UnavailableReason::SamplingFailed, None),
        SampleValue::Unsupported => unavailable(UnavailableReason::Unsupported, None),
    }
}

fn map_process_measurement<T>(
    value: SampleValue<T>,
    sampled_instant: Instant,
    now: Instant,
    observed_seconds: u64,
) -> Measurement<T> {
    if now.saturating_duration_since(sampled_instant) > PROCESS_SAMPLE_STALE_AFTER {
        unavailable(UnavailableReason::ObservationGap, None)
    } else {
        map_measurement(value, observed_seconds)
    }
}

fn unavailable<T>(reason: UnavailableReason, observed_seconds: Option<u64>) -> Measurement<T> {
    Measurement::Unavailable {
        reason,
        observed_seconds,
    }
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// 平台采集到的静态主机身份；无法读取的字段保持 None，不构造占位文本。
#[derive(Default)]
struct HostIdentity {
    os: Option<String>,
    kernel: Option<String>,
    hostname: Option<String>,
}

/// 主机环境：架构、逻辑核心和进程 ID 直接取 std；产品名、内核版本和主机名按平台尽力采集。
fn host_environment() -> HostEnvironment {
    let identity = platform::host_identity();
    HostEnvironment {
        os: identity
            .os
            .map(|value| truncate_host_text(value, HOST_TEXT_MAX_CHARS)),
        kernel: identity
            .kernel
            .map(|value| truncate_host_text(value, HOST_TEXT_MAX_CHARS)),
        arch: std::env::consts::ARCH.to_owned(),
        logical_cores: std::thread::available_parallelism()
            .map(|value| u32::try_from(value.get()).unwrap_or(LOGICAL_CORES_MAX))
            .unwrap_or(1)
            .clamp(1, LOGICAL_CORES_MAX),
        hostname: identity
            .hostname
            .map(|value| truncate_host_text(value, HOSTNAME_MAX_CHARS)),
        process_id: std::process::id(),
    }
}

/// 契约字段有长度上限；超长值截断而不是让响应超出 schema。
fn truncate_host_text(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value
    } else {
        value.chars().take(max_chars).collect()
    }
}

/// 去掉首尾空白；空值返回 None，空字符串不能冒充主机信息。
fn trimmed_host_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// 取 os-release 中指定键的值并去掉可选引号；键缺失或值为空返回 None。
#[cfg(any(target_os = "linux", test))]
fn os_release_value(content: &str, key: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == key).then(|| value.trim().trim_matches('"'))
        })
        .and_then(trimmed_host_value)
}

/// 主机名读取：unix 目标走 POSIX `gethostname`，其他目标只承认环境变量提供的名称。
/// 这里只声明系统 C 库符号；unix 目标默认链接 libc，不新增依赖。
mod host_name {
    #[cfg(unix)]
    pub(super) fn hostname() -> Option<String> {
        /// Linux 内核主机名上限为 64，macOS 为 255；取较大者再多留一个 NUL 位置。
        const CAPACITY: usize = 256;

        unsafe extern "C" {
            fn gethostname(name: *mut core::ffi::c_char, len: usize) -> core::ffi::c_int;
        }

        let mut buffer = [0 as core::ffi::c_char; CAPACITY];
        // SAFETY: 传入本函数持有的可写缓冲区，长度与声明一致；调用失败时缓冲区内容不参与解释。
        if unsafe { gethostname(buffer.as_mut_ptr(), buffer.len()) } != 0 {
            return None;
        }
        let bytes = buffer
            .iter()
            .take_while(|value| **value != 0)
            .map(|value| *value as u8)
            .collect::<Vec<u8>>();
        super::trimmed_host_value(&String::from_utf8_lossy(&bytes))
    }

    #[cfg(not(unix))]
    pub(super) fn hostname() -> Option<String> {
        ["COMPUTERNAME", "HOSTNAME"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok())
            .and_then(|value| super::trimmed_host_value(&value))
    }
}

/// 取 macOS 系统版本 plist 中指定键的字符串值；plist 是 XML，这里只按 key/string 相邻结构取值。
#[cfg(any(not(any(windows, target_os = "linux")), test))]
fn plist_string_value(content: &str, key: &str) -> Option<String> {
    let rest = content.split_once(&format!("<key>{key}</key>"))?.1;
    let value = rest.split_once("<string>")?.1.split_once("</string>")?.0;
    trimmed_host_value(value)
}

#[cfg(windows)]
mod platform {
    use std::mem::size_of;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_EXPAND_SZ, REG_SZ, RegCloseKey,
        RegOpenKeyExW, RegQueryValueExW,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes,
    };

    use super::{HostIdentity, ProcessSample, SampleValue, trimmed_host_value};

    #[derive(Default)]
    pub(super) struct ProcessSampler {
        previous_cpu: Option<(Instant, u64)>,
    }

    impl ProcessSampler {
        pub(super) fn sample(&mut self) -> ProcessSample {
            let process = unsafe { GetCurrentProcess() };
            ProcessSample {
                rss_bytes: sample_rss(process),
                cpu_percent: self.sample_cpu(process),
                threads: sample_threads(),
            }
        }

        fn sample_cpu(&mut self, process: *mut core::ffi::c_void) -> SampleValue<f64> {
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) }
                == 0
            {
                return SampleValue::Failed;
            }
            let now = Instant::now();
            let ticks = filetime_ticks(kernel).saturating_add(filetime_ticks(user));
            match self.previous_cpu.replace((now, ticks)) {
                Some((previous_at, previous_ticks)) => {
                    let elapsed = now.saturating_duration_since(previous_at).as_secs_f64();
                    let consumed = ticks.saturating_sub(previous_ticks) as f64 / 10_000_000.0;
                    if elapsed > 0.0 {
                        SampleValue::Available((consumed / elapsed * 100.0).max(0.0))
                    } else {
                        SampleValue::Warmup
                    }
                }
                None => SampleValue::Warmup,
            }
        }
    }

    fn sample_rss(process: *mut core::ffi::c_void) -> SampleValue<u64> {
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..PROCESS_MEMORY_COUNTERS::default()
        };
        if unsafe { K32GetProcessMemoryInfo(process, &mut counters, counters.cb) } == 0 {
            SampleValue::Failed
        } else {
            SampleValue::Available(counters.WorkingSetSize as u64)
        }
    }

    fn sample_threads() -> SampleValue<u32> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return SampleValue::Failed;
        }
        let snapshot = SnapshotHandle(snapshot);
        let process_id = unsafe { GetCurrentProcessId() };
        let mut entry = PROCESSENTRY32 {
            dwSize: size_of::<PROCESSENTRY32>() as u32,
            ..PROCESSENTRY32::default()
        };
        let mut present = unsafe { Process32First(snapshot.0, &mut entry) } != 0;
        while present {
            if entry.th32ProcessID == process_id {
                return SampleValue::Available(entry.cntThreads);
            }
            present = unsafe { Process32Next(snapshot.0, &mut entry) } != 0;
        }
        SampleValue::Failed
    }

    fn filetime_ticks(value: FILETIME) -> u64 {
        (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
    }

    struct SnapshotHandle(*mut core::ffi::c_void);

    impl Drop for SnapshotHandle {
        fn drop(&mut self) {
            let _ = unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
    /// 产品名、显示版本与构建号来自当前版本注册表键；任一项缺失只影响对应字段。
    pub(super) fn host_identity() -> HostIdentity {
        let mut identity = HostIdentity {
            hostname: super::host_name::hostname(),
            ..HostIdentity::default()
        };
        let path = wide("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
        let mut key: HKEY = core::ptr::null_mut();
        if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
            return identity;
        }
        let key = RegistryKey(key);
        identity.os = match (
            registry_string(key.0, "ProductName"),
            registry_string(key.0, "DisplayVersion"),
        ) {
            (Some(product), Some(display)) => Some(format!("{product} {display}")),
            (product, display) => product.or(display),
        };
        // 主次版本号只在 Windows 10 及以后存在；缺失时宁可留空，也不猜一个版本号。
        identity.kernel = match (
            registry_dword(key.0, "CurrentMajorVersionNumber"),
            registry_dword(key.0, "CurrentMinorVersionNumber"),
            registry_string(key.0, "CurrentBuildNumber"),
        ) {
            (Some(major), Some(minor), Some(build)) => Some(match registry_dword(key.0, "UBR") {
                Some(revision) => format!("{major}.{minor}.{build}.{revision}"),
                None => format!("{major}.{minor}.{build}"),
            }),
            _ => None,
        };
        identity
    }

    fn registry_string(key: HKEY, name: &str) -> Option<String> {
        let name = wide(name);
        let mut data = [0_u16; 256];
        let mut size = (data.len() * size_of::<u16>()) as u32;
        let mut kind = 0_u32;
        let status = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                core::ptr::null_mut(),
                &mut kind,
                data.as_mut_ptr().cast::<u8>(),
                &mut size,
            )
        };
        if status != 0 || !matches!(kind, REG_SZ | REG_EXPAND_SZ) {
            return None;
        }
        let length = (size as usize / size_of::<u16>()).min(data.len());
        trimmed_host_value(String::from_utf16_lossy(&data[..length]).trim_end_matches('\0'))
    }

    fn registry_dword(key: HKEY, name: &str) -> Option<u32> {
        let name = wide(name);
        let mut value = 0_u32;
        let mut size = size_of::<u32>() as u32;
        let mut kind = 0_u32;
        let status = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                core::ptr::null_mut(),
                &mut kind,
                (&mut value as *mut u32).cast::<u8>(),
                &mut size,
            )
        };
        (status == 0 && kind == REG_DWORD).then_some(value)
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(core::iter::once(0)).collect()
    }

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            let _ = unsafe { RegCloseKey(self.0) };
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::time::Instant;

    use super::{HostIdentity, ProcessSample, SampleValue, os_release_value, trimmed_host_value};

    #[derive(Default)]
    pub(super) struct ProcessSampler {
        previous_cpu: Option<(Instant, u64, u64)>,
    }

    impl ProcessSampler {
        pub(super) fn sample(&mut self) -> ProcessSample {
            let status = std::fs::read_to_string("/proc/self/status").ok();
            let rss_bytes = status
                .as_deref()
                .and_then(|value| status_value(value, "VmRSS:"))
                .and_then(|value| value.checked_mul(1024))
                .map_or(SampleValue::Failed, SampleValue::Available);
            let threads = status
                .as_deref()
                .and_then(|value| status_value(value, "Threads:"))
                .and_then(|value| u32::try_from(value).ok())
                .map_or(SampleValue::Failed, SampleValue::Available);
            let cpu_percent = self.sample_cpu();
            ProcessSample {
                rss_bytes,
                cpu_percent,
                threads,
            }
        }

        fn sample_cpu(&mut self) -> SampleValue<f64> {
            let process_ticks = std::fs::read_to_string("/proc/self/stat")
                .ok()
                .and_then(|value| process_ticks(&value));
            let system_ticks = std::fs::read_to_string("/proc/stat")
                .ok()
                .and_then(|value| system_ticks(&value));
            let (Some(process_ticks), Some(system_ticks)) = (process_ticks, system_ticks) else {
                return SampleValue::Failed;
            };
            let now = Instant::now();
            match self
                .previous_cpu
                .replace((now, process_ticks, system_ticks))
            {
                Some((_, previous_process, previous_system)) => {
                    let process_delta = process_ticks.saturating_sub(previous_process) as f64;
                    let system_delta = system_ticks.saturating_sub(previous_system) as f64;
                    if system_delta == 0.0 {
                        SampleValue::Warmup
                    } else {
                        let processors = std::thread::available_parallelism()
                            .map(|value| value.get())
                            .unwrap_or(1) as f64;
                        SampleValue::Available(process_delta / system_delta * processors * 100.0)
                    }
                }
                None => SampleValue::Warmup,
            }
        }
    }

    fn status_value(status: &str, key: &str) -> Option<u64> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    fn process_ticks(stat: &str) -> Option<u64> {
        let fields = stat
            .rsplit_once(") ")?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        let user: u64 = fields.get(11)?.parse().ok()?;
        let system: u64 = fields.get(12)?.parse().ok()?;
        user.checked_add(system)
    }

    fn system_ticks(stat: &str) -> Option<u64> {
        stat.lines()
            .find(|line| line.starts_with("cpu "))?
            .split_whitespace()
            .skip(1)
            .try_fold(0_u64, |total, field| total.checked_add(field.parse().ok()?))
    }
    /// 发行版名称取自 os-release，内核版本取自内核导出的 proc 文件，主机名走 POSIX 调用。
    pub(super) fn host_identity() -> HostIdentity {
        let os = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|content| {
                os_release_value(&content, "PRETTY_NAME")
                    .or_else(|| os_release_value(&content, "NAME"))
            });
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .and_then(|value| trimmed_host_value(&value));
        HostIdentity {
            os,
            kernel,
            hostname: super::host_name::hostname(),
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::{HostIdentity, ProcessSample, SampleValue, plist_string_value};

    #[derive(Default)]
    pub(super) struct ProcessSampler;

    impl ProcessSampler {
        pub(super) fn sample(&mut self) -> ProcessSample {
            ProcessSample {
                rss_bytes: SampleValue::Unsupported,
                cpu_percent: SampleValue::Unsupported,
                threads: SampleValue::Unsupported,
            }
        }
    }
    /// macOS 等平台的产品名与构建号取自系统版本文件；文件缺失时对应字段保持 null。
    pub(super) fn host_identity() -> HostIdentity {
        let version =
            std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok();
        let os = version.as_deref().and_then(|content| {
            let release = plist_string_value(content, "ProductVersion")
                .or_else(|| plist_string_value(content, "ProductUserVisibleVersion"));
            match (plist_string_value(content, "ProductName"), release) {
                (Some(product), Some(release)) => Some(format!("{product} {release}")),
                (product, release) => product.or(release),
            }
        });
        HostIdentity {
            os,
            kernel: version
                .as_deref()
                .and_then(|content| plist_string_value(content, "ProductBuildVersion")),
            hostname: super::host_name::hostname(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant, SystemTime};

    use super::*;
    use crate::dns::ClientId;

    fn client(id: Option<&str>, address: Option<IpAddr>) -> ClientIdentity {
        ClientIdentity {
            client_id: id.map(ClientId::from),
            client_addr: address,
            peer_addr: None,
        }
    }

    #[test]
    fn os_release_and_host_text_keep_only_usable_values() {
        let release = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nVERSION_ID=\"24.04\"\n";
        assert_eq!(
            os_release_value(release, "PRETTY_NAME").as_deref(),
            Some("Ubuntu 24.04.1 LTS")
        );
        assert_eq!(os_release_value(release, "MISSING"), None);
        assert_eq!(os_release_value("NAME=\n", "NAME"), None);
        assert_eq!(
            trimmed_host_value("  fluxdns-win \r\n").as_deref(),
            Some("fluxdns-win")
        );
        assert_eq!(trimmed_host_value("   "), None);
        assert_eq!(truncate_host_text("abcdef".to_owned(), 4), "abcd");
        assert_eq!(truncate_host_text("abc".to_owned(), 4), "abc");
        // macOS 系统版本文件按 sw_vers 口径提供产品名、产品版本与构建号。
        let plist = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>ProductBuildVersion</key>\n\t<string>24C101</string>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n\t<key>ProductVersion</key>\n\t<string>15.2</string>\n</dict>\n</plist>\n";
        assert_eq!(
            plist_string_value(plist, "ProductName").as_deref(),
            Some("macOS")
        );
        assert_eq!(
            plist_string_value(plist, "ProductVersion").as_deref(),
            Some("15.2")
        );
        assert_eq!(
            plist_string_value(plist, "ProductBuildVersion").as_deref(),
            Some("24C101")
        );
        assert_eq!(plist_string_value(plist, "Missing"), None);
    }

    #[test]
    fn host_environment_reports_process_identity() {
        let host = host_environment();
        assert!(!host.arch.is_empty());
        assert!(host.logical_cores >= 1);
        assert_eq!(host.process_id, std::process::id());
        for value in [&host.os, &host.kernel, &host.hostname]
            .into_iter()
            .flatten()
        {
            assert!(!value.trim().is_empty());
        }
    }
    #[test]
    fn fixed_windows_report_warmup_then_exact_rates_and_trends() {
        let started_at = UNIX_EPOCH + Duration::from_secs(1_000);
        let started = Instant::now();
        let owner = MetricsOwner::new_at(started_at, started);
        let address = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
        for second in [0, 1, 59, 60, 599, 600] {
            owner.record_request_at(
                started + Duration::from_secs(second),
                &client(None, Some(address)),
            );
        }

        let warming = owner.service_metrics_at(
            started_at + Duration::from_secs(59),
            started + Duration::from_secs(59),
        );
        assert_eq!(
            warming.qps,
            unavailable(UnavailableReason::Warmup, Some(59))
        );
        assert_eq!(warming.qps_trend.len(), 59);

        let ready = owner.service_metrics_at(
            started_at + Duration::from_secs(600),
            started + Duration::from_secs(600),
        );
        assert_eq!(ready.qps, Measurement::Available { value: 2.0 / 60.0 });
        assert_eq!(ready.rpm, Measurement::Available { value: 0.5 });
        assert_eq!(ready.online_clients, Measurement::Available { value: 1 });
        assert_eq!(ready.qps_trend.len(), 600);
        assert_eq!(
            ready.qps_trend.first().unwrap().value,
            Measurement::Available { value: 1.0 }
        );
        assert_eq!(ready.rpm_trend.len(), 10);
        assert_eq!(
            ready.rpm_trend.last().unwrap().value,
            Measurement::Available { value: 1.0 }
        );

        // 满 660 秒后趋势补齐窗口起点前的 60 个前瞻秒桶，前端据此推导最左一分钟的 RPM。
        let lead = owner.service_metrics_at(
            started_at + Duration::from_secs(660),
            started + Duration::from_secs(660),
        );
        assert_eq!(lead.qps_trend.len(), 660);
        assert_eq!(
            lead.qps_trend.first().unwrap().at_ms,
            unix_millis(started_at)
        );
        assert_eq!(
            lead.qps_trend.last().unwrap().at_ms,
            unix_millis(started_at + Duration::from_secs(659))
        );
    }

    #[test]
    fn metrics_delta_appends_only_new_buckets_and_refuses_after_overflow() {
        let started_at = UNIX_EPOCH + Duration::from_secs(2_000);
        let started = Instant::now();
        let owner = MetricsOwner::new_at(started_at, started);
        let address = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
        for second in 0..=3 {
            owner.record_request_at(
                started + Duration::from_secs(second),
                &client(None, Some(address)),
            );
        }

        // 服务启动不足一秒时基线序列为空、游标停在第 0 秒；此时第一帧增量正好补上序列起点，
        // 管理面前端可以把它当作序列头部直接拼接，无需重新订阅。
        let (empty, start) = owner.service_metrics_with_cursor_at(started_at, started);
        assert!(empty.qps_trend.is_empty());
        assert!(empty.rpm_trend.is_empty());
        assert_eq!(
            start,
            MetricsCursor {
                next_second: 0,
                next_minute: 0
            }
        );
        let (head, head_cursor) = owner
            .service_metrics_delta_at(
                start,
                started_at + Duration::from_secs(1),
                started + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(head.qps_samples[0].at_ms, unix_millis(started_at));
        assert_eq!(head_cursor.next_second, 1);

        // 基线快照覆盖到第 2 秒（含），游标从第 3 秒开始补。
        let (snapshot, cursor) = owner.service_metrics_with_cursor_at(
            started_at + Duration::from_secs(3),
            started + Duration::from_secs(3),
        );
        assert_eq!(snapshot.qps_trend.len(), 3);
        assert_eq!(
            cursor,
            MetricsCursor {
                next_second: 3,
                next_minute: 0
            }
        );

        // 一秒后只补第 3 秒这一个新桶，并与基线末桶整秒连续，客户端可以直接追加。
        let (delta, next) = owner
            .service_metrics_delta_at(
                cursor,
                started_at + Duration::from_secs(4),
                started + Duration::from_secs(4),
            )
            .unwrap();
        assert_eq!(
            next,
            MetricsCursor {
                next_second: 4,
                next_minute: 0
            }
        );
        assert_eq!(delta.qps_samples.len(), 1);
        assert_eq!(
            delta.qps_samples[0].at_ms,
            unix_millis(started_at + Duration::from_secs(3))
        );
        assert_eq!(
            snapshot.qps_trend.last().unwrap().at_ms + 1_000,
            delta.qps_samples[0].at_ms
        );
        assert_eq!(
            delta.qps_samples[0].value,
            Measurement::Available { value: 1.0 }
        );
        assert!(delta.rpm_samples.is_empty());

        // 跨过整分钟后增量带上新完成的分钟桶，并覆盖两个新秒桶。
        let (_, boundary) = owner.service_metrics_with_cursor_at(
            started_at + Duration::from_secs(59),
            started + Duration::from_secs(59),
        );
        let (delta, next) = owner
            .service_metrics_delta_at(
                boundary,
                started_at + Duration::from_secs(61),
                started + Duration::from_secs(61),
            )
            .unwrap();
        assert_eq!(delta.qps_samples.len(), 2);
        assert_eq!(next.next_minute, 1);
        assert_eq!(delta.rpm_samples.len(), 1);
        assert_eq!(delta.rpm_samples[0].at_ms, unix_millis(started_at));
        assert_eq!(
            delta.rpm_samples[0].value,
            Measurement::Available { value: 4.0 }
        );

        // 停滞超过一个完整保留窗口时不能补洞：必须让调用方重发全量基线。
        assert!(
            owner
                .service_metrics_delta_at(
                    cursor,
                    started_at + Duration::from_secs(700),
                    started + Duration::from_secs(700),
                )
                .is_none()
        );
    }

    #[test]
    fn online_identity_prefers_id_and_normalizes_mapped_ipv4() {
        let started_at = SystemTime::now();
        let started = Instant::now();
        let owner = MetricsOwner::new_at(started_at, started);
        let ipv4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 10).to_ipv6_mapped());
        for identity in [
            client(Some("alpha"), Some(ipv4)),
            client(Some("beta"), Some(ipv4)),
            client(
                Some("alpha"),
                Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4))),
            ),
            client(None, Some(ipv4)),
            client(None, Some(mapped)),
            client(None, None),
        ] {
            owner.record_request_at(started + Duration::from_secs(60), &identity);
        }
        let metrics = owner.service_metrics_at(
            started_at + Duration::from_secs(60),
            started + Duration::from_secs(60),
        );
        assert_eq!(metrics.online_clients, Measurement::Available { value: 3 });
    }

    #[test]
    fn online_capacity_reports_gap_until_truncated_window_expires() {
        let started_at = SystemTime::now();
        let started = Instant::now();
        let owner = MetricsOwner::new_at(started_at, started);
        for index in 0..=MAX_ONLINE_IDENTITIES {
            owner.record_request_at(
                started + Duration::from_secs(60),
                &client(Some(&format!("client-{index}")), None),
            );
        }
        let overflow = owner.service_metrics_at(
            started_at + Duration::from_secs(60),
            started + Duration::from_secs(60),
        );
        assert_eq!(
            overflow.online_clients,
            unavailable(UnavailableReason::ObservationGap, None)
        );
        let recovered = owner.service_metrics_at(
            started_at + Duration::from_secs(120),
            started + Duration::from_secs(120),
        );
        assert_eq!(
            recovered.online_clients,
            Measurement::Available { value: 0 }
        );
    }

    #[test]
    fn service_metrics_reuse_process_snapshot_for_cpu() {
        let started_at = SystemTime::now();
        let started = Instant::now();
        let owner = MetricsOwner::new_at(started_at, started);
        owner.set_process_sample_for_test(1024, 2.5, 4);

        let metrics = owner.service_metrics_at(
            started_at + Duration::from_secs(1),
            started + Duration::from_secs(1),
        );
        assert_eq!(metrics.cpu_percent, Measurement::Available { value: 2.5 });
    }

    #[test]
    fn stale_process_snapshot_reports_observation_gap() {
        let started_at = SystemTime::now();
        let started = Instant::now();
        let owner = MetricsOwner::new_at(started_at, started);
        owner.publish_process_sample(
            started_at,
            started,
            ProcessSample {
                rss_bytes: SampleValue::Available(1024),
                cpu_percent: SampleValue::Available(2.5),
                threads: SampleValue::Available(4),
            },
        );

        let metrics = owner.process_metrics_at(started + Duration::from_secs(4));
        assert!(matches!(
            metrics.rss_bytes,
            Measurement::Unavailable {
                reason: UnavailableReason::ObservationGap,
                observed_seconds: None,
            }
        ));
        assert_eq!(
            metrics.cpu_percent,
            unavailable(UnavailableReason::ObservationGap, None)
        );
        assert_eq!(
            metrics.threads,
            unavailable(UnavailableReason::ObservationGap, None)
        );

        // 服务状态快照复用同一进程样本，超时后 CPU 与 RSS 同样按 observation_gap 降级。
        let service = owner.service_metrics_at(
            started_at + Duration::from_secs(4),
            started + Duration::from_secs(4),
        );
        assert!(matches!(
            service.rss_bytes,
            Measurement::Unavailable {
                reason: UnavailableReason::ObservationGap,
                observed_seconds: None,
            }
        ));
        assert_eq!(
            service.cpu_percent,
            unavailable(UnavailableReason::ObservationGap, None)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_sampler_reads_real_rss_cpu_and_threads() {
        let mut sampler = platform::ProcessSampler::default();
        let first = sampler.sample();
        assert!(matches!(first.rss_bytes, SampleValue::Available(value) if value > 0));
        assert!(matches!(first.threads, SampleValue::Available(value) if value > 0));
        assert_eq!(first.cpu_percent, SampleValue::Warmup);
        std::thread::sleep(Duration::from_millis(50));
        let second = sampler.sample();
        assert!(
            matches!(second.cpu_percent, SampleValue::Available(value) if value.is_finite() && value >= 0.0)
        );
    }
}
