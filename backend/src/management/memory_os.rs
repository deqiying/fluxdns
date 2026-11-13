//! 有界 OS 内存采样与真实样本历史；不访问业务对象。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::contract::{DecimalU64, Measurement, MemoryOsSample, UnavailableReason};

pub(super) const INTERVAL_SECONDS: u32 = 30;
pub(super) const GAP_SECONDS: u32 = 75;
const HISTORY_SECONDS: u64 = 3600;
const HISTORY_CAPACITY: usize = 121;

pub(super) fn unavailable(reason: UnavailableReason) -> Measurement<DecimalU64> {
    Measurement::Unavailable {
        reason,
        observed_seconds: None,
    }
}

impl MemoryOsSample {
    pub(super) fn unavailable(sampled_at_ms: u64, reason: UnavailableReason) -> Self {
        let value = unavailable(reason);
        Self {
            sampled_at_ms,
            rss_bytes: value.clone(),
            peak_rss_bytes: value.clone(),
            anonymous_bytes: value.clone(),
            file_bytes: value.clone(),
            shared_bytes: value.clone(),
            pss_bytes: value.clone(),
            private_dirty_bytes: value.clone(),
            swap_bytes: value,
        }
    }
}

pub(super) struct MemoryHistory {
    latest: MemoryOsSample,
    last_instant: Option<Instant>,
    samples: VecDeque<(Instant, MemoryOsSample)>,
}

impl MemoryHistory {
    pub(super) fn new(at_ms: u64) -> Self {
        Self {
            latest: MemoryOsSample::unavailable(at_ms, UnavailableReason::Warmup),
            last_instant: None,
            samples: VecDeque::new(),
        }
    }

    pub(super) fn due(&self, now: Instant) -> bool {
        self.last_instant.is_none_or(|last| {
            now.saturating_duration_since(last) >= Duration::from_secs(INTERVAL_SECONDS.into())
        })
    }

    pub(super) fn push(&mut self, now: Instant, sample: MemoryOsSample) {
        // 拒绝晚到/倒序时钟样本，避免旧结果覆盖新状态；重启由新的 owner 隔离。
        if self.last_instant.is_some_and(|last| now <= last) {
            return;
        }
        self.last_instant = Some(now);
        self.latest = sample.clone();
        self.samples.push_back((now, sample));
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        while self.samples.front().is_some_and(|(at, _)| {
            now.saturating_duration_since(*at) > Duration::from_secs(HISTORY_SECONDS)
        }) || self.samples.len() > HISTORY_CAPACITY
        {
            self.samples.pop_front();
        }
    }

    pub(super) fn read(&mut self, now: Instant) -> (MemoryOsSample, Vec<MemoryOsSample>) {
        self.prune(now);
        let latest = if self.last_instant.is_some_and(|last| {
            now.saturating_duration_since(last) > Duration::from_secs(GAP_SECONDS.into())
        }) {
            MemoryOsSample::unavailable(
                self.latest.sampled_at_ms,
                UnavailableReason::ObservationGap,
            )
        } else {
            self.latest.clone()
        };
        (
            latest,
            self.samples
                .iter()
                .map(|(_, sample)| sample.clone())
                .collect(),
        )
    }
}

/// proc 的 kB 固定按 1024 bytes 解释；字段缺失、重复、单位/格式异常和溢出独立降级。
#[cfg(any(target_os = "linux", test))]
fn proc_bytes(content: Option<&str>, key: &str) -> Measurement<DecimalU64> {
    let parsed = (|| {
        let mut matches = content?.lines().filter_map(|line| line.strip_prefix(key));
        let value = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let mut fields = value.split_whitespace();
        let digits = fields.next()?;
        if !digits.bytes().all(|byte| byte.is_ascii_digit())
            || fields.next()? != "kB"
            || fields.next().is_some()
        {
            return None;
        }
        digits.parse::<u64>().ok()?.checked_mul(1024)
    })();
    parsed.map_or_else(
        || unavailable(UnavailableReason::SamplingFailed),
        |value| Measurement::Available {
            value: value.into(),
        },
    )
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_linux(
    at_ms: u64,
    status: Option<&str>,
    rollup: Option<&str>,
) -> MemoryOsSample {
    MemoryOsSample {
        sampled_at_ms: at_ms,
        rss_bytes: proc_bytes(status, "VmRSS:"),
        peak_rss_bytes: proc_bytes(status, "VmHWM:"),
        anonymous_bytes: proc_bytes(status, "RssAnon:"),
        file_bytes: proc_bytes(status, "RssFile:"),
        shared_bytes: proc_bytes(status, "RssShmem:"),
        pss_bytes: proc_bytes(rollup, "Pss:"),
        private_dirty_bytes: proc_bytes(rollup, "Private_Dirty:"),
        swap_bytes: proc_bytes(rollup, "Swap:"),
    }
}

/// 内核文件仍使用硬上限，异常文件或读取失败不扩大采样内存。
#[cfg(target_os = "linux")]
pub(super) fn read_rollup() -> Option<String> {
    use std::io::Read;
    const MAX_BYTES: u64 = 64 * 1024;
    let mut data = String::new();
    std::fs::File::open("/proc/self/smaps_rollup")
        .ok()?
        .take(MAX_BYTES + 1)
        .read_to_string(&mut data)
        .ok()?;
    (data.len() as u64 <= MAX_BYTES).then_some(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(value: &Measurement<DecimalU64>) -> Option<u64> {
        match value {
            Measurement::Available { value } => Some(value.as_u64()),
            _ => None,
        }
    }

    #[test]
    fn proc_fields_have_strict_units_and_independent_availability() {
        let status = "VmRSS: 129464 kB\nVmHWM: 129464 kB\nRssAnon: 110912 kB\nRssFile: 18552 kB\nRssShmem: 0 kB\n";
        let sample = parse_linux(
            7,
            Some(status),
            Some("Pss: 128464 kB\nPrivate_Dirty: 110944 kB\nSwap: 0 kB\n"),
        );
        assert_eq!(bytes(&sample.rss_bytes), Some(129464 * 1024));
        assert_eq!(bytes(&sample.anonymous_bytes), Some(110912 * 1024));
        assert_eq!(bytes(&sample.pss_bytes), Some(128464 * 1024));
        assert_eq!(bytes(&sample.shared_bytes), Some(0));
        for invalid in [
            "VmRSS: 1 MB",
            "VmRSS: -1 kB",
            "VmRSS: 18446744073709551615 kB",
            "VmRSS: 2 kB trailing",
            "VmRSS: 1 kB\nVmRSS: 2 kB",
            "VmRSS:",
        ] {
            assert_eq!(
                bytes(&parse_linux(1, Some(invalid), None).rss_bytes),
                None,
                "{invalid}"
            );
        }
        let partial = parse_linux(1, Some("VmRSS: 9 kB\nRssAnon: 20 kB"), None);
        assert_eq!(bytes(&partial.rss_bytes), Some(9216));
        assert_eq!(bytes(&partial.anonymous_bytes), Some(20480)); // 不修正真实读取差异。
        assert_eq!(bytes(&partial.pss_bytes), None);
    }

    #[test]
    fn history_is_bounded_stale_and_separate_per_owner() {
        let start = Instant::now();
        let mut history = MemoryHistory::new(0);
        assert!(history.due(start));
        for index in 0..130 {
            history.push(
                start + Duration::from_secs(index * 30),
                MemoryOsSample::unavailable(index * 30000, UnavailableReason::SamplingFailed),
            );
        }
        let now = start + Duration::from_secs(129 * 30);
        let (latest, trend) = history.read(now);
        assert_eq!(trend.len(), 121);
        assert_eq!(latest.sampled_at_ms, 129 * 30000);
        history.push(
            start,
            MemoryOsSample::unavailable(99, UnavailableReason::Warmup),
        );
        assert_eq!(history.read(now).0.sampled_at_ms, latest.sampled_at_ms);
        assert!(matches!(
            history.read(now + Duration::from_secs(76)).0.rss_bytes,
            Measurement::Unavailable {
                reason: UnavailableReason::ObservationGap,
                ..
            }
        ));
        assert!(MemoryHistory::new(1).read(now).1.is_empty());
    }
}
