//! Management 服务指标的进程级采集 owner。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::contract::{
    DecimalU64, MAX_ONLINE_IDENTITIES, Measurement, ProcessMetrics, RateSample, ServiceMetrics,
    UnavailableReason,
};
use crate::dns::{Cancellation, ClientIdentity};
use crate::runtime::TaskError;

const REQUEST_WINDOW_SECONDS: u64 = 600;
const REQUEST_BUCKET_COUNT: usize = REQUEST_WINDOW_SECONDS as usize + 1;
const QPS_WINDOW_SECONDS: u64 = 60;
const RPM_WINDOW_SECONDS: u64 = 600;
const ONLINE_WINDOW_SECONDS: u64 = 60;
const PROCESS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const PROCESS_SAMPLE_STALE_AFTER: Duration = Duration::from_secs(3);

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

    fn service_metrics_at(&self, sampled_at: SystemTime, now: Instant) -> ServiceMetrics {
        let elapsed = self.elapsed_seconds(now);
        let sampled_at_ms = unix_millis(sampled_at);
        let mut state = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .online
            .retain(|_, last_seen| elapsed.saturating_sub(*last_seen) < ONLINE_WINDOW_SECONDS);

        let qps = window_measurement(
            elapsed,
            QPS_WINDOW_SECONDS,
            count_window(&state, elapsed, QPS_WINDOW_SECONDS) as f64 / QPS_WINDOW_SECONDS as f64,
        );
        let rpm = window_measurement(
            elapsed,
            RPM_WINDOW_SECONDS,
            count_window(&state, elapsed, RPM_WINDOW_SECONDS) as f64 / 10.0,
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
        let process = *self
            .process
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        ServiceMetrics {
            sampled_at_ms,
            qps,
            rpm,
            online_clients,
            rss_bytes: map_process_bytes(process, now, elapsed),
            qps_trend: qps_trend(&state, self.started_at, elapsed),
            rpm_trend: rpm_trend(&state, self.started_at, elapsed),
        }
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

fn qps_trend(state: &RequestState, started_at: SystemTime, elapsed: u64) -> Vec<RateSample> {
    let completed = elapsed.min(REQUEST_WINDOW_SECONDS);
    let first = elapsed.saturating_sub(completed);
    (first..elapsed)
        .map(|second| RateSample {
            at_ms: unix_millis(started_at + Duration::from_secs(second)),
            value: Measurement::Available {
                value: count_at(state, second) as f64,
            },
        })
        .collect()
}

fn rpm_trend(state: &RequestState, started_at: SystemTime, elapsed: u64) -> Vec<RateSample> {
    let completed_minutes = (elapsed / 60).min(10);
    let first_minute = elapsed / 60 - completed_minutes;
    (first_minute..elapsed / 60)
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
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes,
    };

    use super::{ProcessSample, SampleValue};

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
            let value = match self.previous_cpu.replace((now, ticks)) {
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
            };
            value
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
}

#[cfg(target_os = "linux")]
mod platform {
    use std::time::Instant;

    use super::{ProcessSample, SampleValue};

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
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::{ProcessSample, SampleValue};

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
