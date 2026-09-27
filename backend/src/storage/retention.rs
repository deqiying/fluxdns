//! 统计与详情共用的单调保留水位计算、发布和恢复边界。

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::dns::Cancellation;
use crate::dns::Deadline;
use crate::ports::{PortError, PortErrorClass};

use super::{
    DetailShardStore, SqliteStorageBackend, StatsPersistenceError, StatsPersistenceWorker,
};

pub const DEFAULT_RETENTION_DAYS: u32 = 7;
pub const DEFAULT_RETENTION_GRACE_DAYS: u32 = 3;
pub const DEFAULT_RETENTION_REFERENCE_SIZE_BYTES: u64 = 1 << 30;
pub const MAX_RETENTION_DAYS: u32 = 3_650;
pub const MAX_RETENTION_REFERENCE_SIZE_BYTES: u64 = 1 << 40;
pub const RETENTION_SCHEDULE_LOCAL_SECOND: u32 = 60 * 60;
pub const DEFAULT_RETENTION_POLL_INTERVAL: Duration = Duration::from_secs(60);
pub const DEFAULT_RETENTION_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_RETENTION_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub days: u32,
    pub grace_days: u32,
    pub reference_size_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            days: DEFAULT_RETENTION_DAYS,
            grace_days: DEFAULT_RETENTION_GRACE_DAYS,
            reference_size_bytes: DEFAULT_RETENTION_REFERENCE_SIZE_BYTES,
        }
    }
}

impl RetentionPolicy {
    pub fn new(
        days: u32,
        grace_days: u32,
        reference_size_bytes: u64,
    ) -> Result<Self, RetentionPolicyError> {
        if days == 0
            || days > MAX_RETENTION_DAYS
            || days
                .checked_add(grace_days)
                .is_none_or(|total| total > MAX_RETENTION_DAYS)
            || !(1..=MAX_RETENTION_REFERENCE_SIZE_BYTES).contains(&reference_size_bytes)
        {
            return Err(RetentionPolicyError::Invalid);
        }
        Ok(Self {
            days,
            grace_days,
            reference_size_bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum RetentionPolicyError {
    #[error("invalid retention policy")]
    Invalid,
    #[error("retention cutoff is outside the supported UTC day range")]
    DayOutOfRange,
    #[error("sampled detail size exceeds the persisted integer range")]
    SampleTooLarge,
}

/// 一轮任务冻结的输入和计算结果；`S == T` 仍使用宽限期。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPlan {
    pub policy: RetentionPolicy,
    pub reference_day_utc: i32,
    pub sampled_detail_bytes: u64,
    pub target_days: u32,
    pub keep_from_day_utc: i32,
}

impl RetentionPlan {
    pub fn calculate(
        policy: RetentionPolicy,
        reference_day_utc: i32,
        sampled_detail_bytes: u64,
    ) -> Result<Self, RetentionPolicyError> {
        let policy =
            RetentionPolicy::new(policy.days, policy.grace_days, policy.reference_size_bytes)?;
        if i64::try_from(sampled_detail_bytes).is_err() {
            return Err(RetentionPolicyError::SampleTooLarge);
        }
        let target_days = if sampled_detail_bytes > policy.reference_size_bytes {
            policy.days
        } else {
            policy
                .days
                .checked_add(policy.grace_days)
                .ok_or(RetentionPolicyError::Invalid)?
        };
        let days_before = i32::try_from(target_days.saturating_sub(1))
            .map_err(|_| RetentionPolicyError::DayOutOfRange)?;
        let keep_from_day_utc = reference_day_utc
            .checked_sub(days_before)
            .ok_or(RetentionPolicyError::DayOutOfRange)?;
        // 复用分片日期编码范围，避免生成永远无法定位的水位。
        if super::detail_shards::format_shard_file_name(reference_day_utc).is_none()
            || super::detail_shards::format_shard_file_name(keep_from_day_utc).is_none()
        {
            return Err(RetentionPolicyError::DayOutOfRange);
        }
        Ok(Self {
            policy,
            reference_day_utc,
            sampled_detail_bytes,
            target_days,
            keep_from_day_utc,
        })
    }

    pub(crate) fn is_valid(&self) -> bool {
        Self::calculate(
            self.policy,
            self.reference_day_utc,
            self.sampled_detail_bytes,
        )
        .is_ok_and(|expected| expected == *self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionState {
    pub revision: u64,
    pub watermark_revision: u64,
    pub retired_before_day_utc: i32,
    pub reference_day_utc: i32,
    pub target_days: u32,
    pub sampled_detail_bytes: u64,
    pub reference_size_bytes: u64,
    pub replay_floor_batch_id: u64,
    pub published_at: SystemTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionManifestState {
    Pending,
    Reclaimed,
    Failed,
}

impl RetentionManifestState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Reclaimed => "reclaimed",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "reclaimed" => Some(Self::Reclaimed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionManifestEntry {
    pub day_utc: i32,
    pub retired_revision: u64,
    pub state: RetentionManifestState,
    pub attempts: u32,
    pub last_error_code: Option<String>,
    pub updated_at: SystemTime,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetentionRunState {
    pub last_attempt_local_day: Option<i32>,
    pub last_success_local_day: Option<i32>,
    pub last_attempted_at: Option<SystemTime>,
    pub last_succeeded_at: Option<SystemTime>,
    pub consecutive_failures: u32,
    pub last_error_code: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionAvailableRange {
    pub from_day_utc: Option<i32>,
    pub to_day_utc: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionStatus {
    pub policy: RetentionPolicy,
    pub target_days: u32,
    pub sampled_detail_bytes: u64,
    pub published: Option<RetentionState>,
    pub next_expected_retired_before_day_utc: i32,
    pub detail_available: RetentionAvailableRange,
    pub stats_available: RetentionAvailableRange,
    pub pending_reclaims: u32,
    pub failed_reclaims: u32,
    pub pending_reclaim_bytes: u64,
    pub last_cleanup_at: Option<SystemTime>,
    pub run: RetentionRunState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetentionStatusMetadata {
    pub published: Option<RetentionState>,
    pub stats_available: RetentionAvailableRange,
    pub pending_reclaims: u32,
    pub failed_reclaims: u32,
    pub last_reclaim_at: Option<SystemTime>,
    pub run: RetentionRunState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionReclaimSummary {
    pub attempted: u32,
    pub reclaimed: u32,
    pub failed: u32,
    pub reclaimed_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionSchedulerSummary {
    pub daily_runs: u64,
    pub daily_failures: u64,
    pub reclaim_runs: u64,
    pub reclaim_failures: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetentionWallTime {
    pub observed_at: SystemTime,
    pub local_day: i32,
    pub local_second: u32,
    pub utc_day: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetentionBootstrap {
    pub state: Option<RetentionState>,
    pub next_stats_batch_id: u64,
}

#[derive(Debug, Error)]
pub enum RetentionError {
    #[error("retention policy calculation failed: {0}")]
    Policy(#[source] RetentionPolicyError),
    #[error("retention detail sampling failed: {0}")]
    Sample(#[source] PortError),
    #[error("retention stats boundary failed: {0}")]
    Stats(#[source] StatsPersistenceError),
    #[error("retention detail boundary failed: {0}")]
    Detail(#[source] PortError),
    #[error("retention state transaction failed: {0}")]
    Backend(#[source] PortError),
}

impl RetentionError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Policy(_) => "policy",
            Self::Sample(_) => "sample",
            Self::Stats(_) => "stats",
            Self::Detail(_) => "detail",
            Self::Backend(_) => "backend",
        }
    }
}

/// 共同水位、manifest 回收、状态查询和每日 scheduler 共用的进程级协调边界。
pub struct RetentionCoordinator {
    backend: Arc<SqliteStorageBackend>,
    stats: Arc<StatsPersistenceWorker>,
    detail: Arc<DetailShardStore>,
    run_lock: tokio::sync::Mutex<()>,
}

impl RetentionCoordinator {
    pub(crate) fn new(
        backend: Arc<SqliteStorageBackend>,
        stats: Arc<StatsPersistenceWorker>,
        detail: Arc<DetailShardStore>,
    ) -> Self {
        Self {
            backend,
            stats,
            detail,
            run_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 仅采样并计算，不发布水位或创建任务记录。
    pub async fn preview(
        &self,
        policy: RetentionPolicy,
        reference_day_utc: i32,
        deadline: Deadline,
    ) -> Result<RetentionPlan, RetentionError> {
        let sample = self
            .detail
            .sample_managed_storage(deadline)
            .map_err(RetentionError::Sample)?;
        RetentionPlan::calculate(policy, reference_day_utc, sample.bytes)
            .map_err(RetentionError::Policy)
    }

    /// 发布 stats 事务水位；成功后在同一详情 write guard 下更新进程可见边界。
    pub async fn publish(
        &self,
        plan: RetentionPlan,
        deadline: Deadline,
    ) -> Result<RetentionState, RetentionError> {
        let _run = tokio::time::timeout(deadline.remaining(Instant::now()), self.run_lock.lock())
            .await
            .map_err(|_| {
                RetentionError::Backend(PortError::new(
                    PortErrorClass::Timeout,
                    "retention.run_lock",
                ))
            })?;
        let detail = self
            .detail
            .begin_retention_publication(deadline)
            .await
            .map_err(RetentionError::Detail)?;
        // 在 write guard 内重新枚举文件，避免预览后完成的迟到写入漏入 manifest；S 和截止线仍使用冻结 plan。
        let manifest_days = self
            .detail
            .sample_managed_storage(deadline)
            .map_err(RetentionError::Sample)?
            .shard_days;
        let stats = self
            .stats
            .begin_retention(deadline)
            .await
            .map_err(RetentionError::Stats)?;
        let state = self
            .backend
            .publish_retention(
                plan,
                &manifest_days,
                stats.replay_floor_batch_id(),
                deadline,
            )
            .await
            .map_err(RetentionError::Backend)?;
        detail.publish(state.retired_before_day_utc);
        stats.commit();
        Ok(state)
    }

    /// 在同一 run lock 和详情 write guard 中采样、计算并发布每天的共同水位。
    pub async fn run_daily(
        &self,
        policy: RetentionPolicy,
        reference_day_utc: i32,
        deadline: Deadline,
    ) -> Result<RetentionState, RetentionError> {
        let _run = tokio::time::timeout(deadline.remaining(Instant::now()), self.run_lock.lock())
            .await
            .map_err(|_| {
                RetentionError::Backend(PortError::new(
                    PortErrorClass::Timeout,
                    "retention.run_lock",
                ))
            })?;
        let detail = self
            .detail
            .begin_retention_publication(deadline)
            .await
            .map_err(RetentionError::Detail)?;
        let sample = self
            .detail
            .sample_managed_storage(deadline)
            .map_err(RetentionError::Sample)?;
        let plan = RetentionPlan::calculate(policy, reference_day_utc, sample.bytes)
            .map_err(RetentionError::Policy)?;
        let stats = self
            .stats
            .begin_retention(deadline)
            .await
            .map_err(RetentionError::Stats)?;
        let state = self
            .backend
            .publish_retention(
                plan,
                &sample.shard_days,
                stats.replay_floor_batch_id(),
                deadline,
            )
            .await
            .map_err(RetentionError::Backend)?;
        detail.publish(state.retired_before_day_utc);
        stats.commit();
        Ok(state)
    }

    /// 重试所有 pending/failed manifest；单日失败会持久化且不阻断其余日期。
    pub async fn reclaim_pending(
        &self,
        deadline: Deadline,
    ) -> Result<RetentionReclaimSummary, RetentionError> {
        let _run = tokio::time::timeout(deadline.remaining(Instant::now()), self.run_lock.lock())
            .await
            .map_err(|_| {
                RetentionError::Backend(PortError::new(
                    PortErrorClass::Timeout,
                    "retention.run_lock",
                ))
            })?;
        let entries = self
            .backend
            .pending_retention_reclaims(deadline)
            .await
            .map_err(RetentionError::Backend)?;
        let mut summary = RetentionReclaimSummary::default();
        for entry in entries {
            if deadline.is_expired(Instant::now()) {
                return Err(RetentionError::Detail(PortError::new(
                    PortErrorClass::Timeout,
                    "retention.reclaim",
                )));
            }
            summary.attempted = summary.attempted.saturating_add(1);
            let result = match self.detail.begin_retirement(entry.day_utc, deadline).await {
                Ok(lease) => lease.reclaim(deadline).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(bytes) => {
                    self.backend
                        .finish_retention_reclaim(entry.day_utc, None, deadline)
                        .await
                        .map_err(RetentionError::Backend)?;
                    summary.reclaimed = summary.reclaimed.saturating_add(1);
                    summary.reclaimed_bytes = summary.reclaimed_bytes.saturating_add(bytes);
                }
                Err(error) => {
                    let code = reclaim_error_code(&error);
                    self.backend
                        .finish_retention_reclaim(entry.day_utc, Some(code), deadline)
                        .await
                        .map_err(RetentionError::Backend)?;
                    summary.failed = summary.failed.saturating_add(1);
                }
            }
        }
        Ok(summary)
    }

    /// 返回下一轮计划、已发布水位、实际可查范围和 manifest/run 状态。
    pub async fn status(
        &self,
        policy: RetentionPolicy,
        reference_day_utc: i32,
        deadline: Deadline,
    ) -> Result<RetentionStatus, RetentionError> {
        let _run = tokio::time::timeout(deadline.remaining(Instant::now()), self.run_lock.lock())
            .await
            .map_err(|_| {
                RetentionError::Backend(PortError::new(
                    PortErrorClass::Timeout,
                    "retention.run_lock",
                ))
            })?;
        let sample = self
            .detail
            .sample_managed_storage(deadline)
            .map_err(RetentionError::Sample)?;
        let plan = RetentionPlan::calculate(policy, reference_day_utc, sample.bytes)
            .map_err(RetentionError::Policy)?;
        let metadata = self
            .backend
            .retention_status_metadata(deadline)
            .await
            .map_err(RetentionError::Backend)?;
        let pending_days = self
            .backend
            .pending_retention_reclaims(deadline)
            .await
            .map_err(RetentionError::Backend)?
            .into_iter()
            .map(|entry| entry.day_utc)
            .collect::<BTreeSet<_>>();
        let pending_reclaim_bytes = self
            .detail
            .sample_pending_storage(&pending_days, deadline)
            .map_err(RetentionError::Sample)?
            .bytes;
        let published_cutoff = metadata.published.map(|state| state.retired_before_day_utc);
        let visible_days = sample
            .shard_days
            .into_iter()
            .filter(|day| published_cutoff.is_none_or(|cutoff| *day >= cutoff));
        let detail_available = available_range(visible_days);
        Ok(RetentionStatus {
            policy,
            target_days: plan.target_days,
            sampled_detail_bytes: plan.sampled_detail_bytes,
            published: metadata.published,
            next_expected_retired_before_day_utc: published_cutoff
                .map_or(plan.keep_from_day_utc, |cutoff| {
                    cutoff.max(plan.keep_from_day_utc)
                }),
            detail_available,
            stats_available: metadata.stats_available,
            pending_reclaims: metadata.pending_reclaims,
            failed_reclaims: metadata.failed_reclaims,
            pending_reclaim_bytes,
            last_cleanup_at: metadata
                .last_reclaim_at
                .into_iter()
                .chain(metadata.run.last_succeeded_at)
                .max(),
            run: metadata.run,
        })
    }
}

pub(crate) struct RetentionScheduler {
    coordinator: Arc<RetentionCoordinator>,
    policy: RetentionPolicy,
    poll_interval: Duration,
    retry_interval: Duration,
    operation_timeout: Duration,
}

impl RetentionScheduler {
    pub(crate) fn new(coordinator: Arc<RetentionCoordinator>, policy: RetentionPolicy) -> Self {
        Self {
            coordinator,
            policy,
            poll_interval: DEFAULT_RETENTION_POLL_INTERVAL,
            retry_interval: DEFAULT_RETENTION_RETRY_INTERVAL,
            operation_timeout: DEFAULT_RETENTION_OPERATION_TIMEOUT,
        }
    }

    /// 启动时立即核对补跑，之后短周期重读墙钟和系统时区，不固定 sleep 24h。
    pub(crate) async fn run_until_stopped(
        self,
        cancellation: Cancellation,
    ) -> RetentionSchedulerSummary {
        let mut summary = RetentionSchedulerSummary::default();
        let mut retry_after = None;
        loop {
            let monotonic_now = Instant::now();
            match system_wall_time() {
                Ok(wall) => {
                    let deadline = Deadline::new(monotonic_now + self.operation_timeout);
                    let tick = self
                        .tick(
                            wall,
                            retry_after.is_none_or(|at| monotonic_now >= at),
                            deadline,
                        )
                        .await;
                    match tick {
                        Ok(outcome) => {
                            summary.daily_runs =
                                summary.daily_runs.saturating_add(outcome.daily_runs);
                            summary.daily_failures = summary
                                .daily_failures
                                .saturating_add(outcome.daily_failures);
                            summary.reclaim_runs =
                                summary.reclaim_runs.saturating_add(outcome.reclaim_runs);
                            summary.reclaim_failures = summary
                                .reclaim_failures
                                .saturating_add(outcome.reclaim_failures);
                            if retry_after.is_none_or(|at| monotonic_now >= at) {
                                retry_after = outcome
                                    .needs_retry
                                    .then_some(monotonic_now + self.retry_interval);
                            }
                        }
                        Err(_) => retry_after = Some(monotonic_now + self.retry_interval),
                    }
                }
                Err(_) => retry_after = Some(monotonic_now + self.retry_interval),
            }
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
        }
        summary
    }

    pub(crate) async fn tick(
        &self,
        wall: RetentionWallTime,
        retry_allowed: bool,
        deadline: Deadline,
    ) -> Result<RetentionSchedulerTick, RetentionError> {
        self.coordinator
            .backend
            .initialize_retention_schedule(wall.local_day, wall.local_second, deadline)
            .await
            .map_err(RetentionError::Backend)?;
        let run = self
            .coordinator
            .backend
            .retention_run_state(deadline)
            .await
            .map_err(RetentionError::Backend)?;
        let daily_due = daily_run_is_due(run.last_success_local_day, wall);
        let mut outcome = RetentionSchedulerTick::default();
        if daily_due && retry_allowed {
            outcome.daily_runs = 1;
            self.coordinator
                .backend
                .begin_retention_run(wall.local_day, wall.observed_at, deadline)
                .await
                .map_err(RetentionError::Backend)?;
            match self
                .coordinator
                .run_daily(self.policy, wall.utc_day, deadline)
                .await
            {
                Ok(_) => {
                    self.coordinator
                        .backend
                        .finish_retention_run(wall.local_day, wall.observed_at, None, deadline)
                        .await
                        .map_err(RetentionError::Backend)?;
                }
                Err(error) => {
                    self.coordinator
                        .backend
                        .finish_retention_run(
                            wall.local_day,
                            wall.observed_at,
                            Some(error.code()),
                            deadline,
                        )
                        .await
                        .map_err(RetentionError::Backend)?;
                    outcome.daily_failures = 1;
                    outcome.needs_retry = true;
                    return Ok(outcome);
                }
            }
        }
        let pending = self
            .coordinator
            .backend
            .pending_retention_reclaim_count(deadline)
            .await
            .map_err(RetentionError::Backend)?;
        if pending > 0 && retry_allowed {
            outcome.reclaim_runs = 1;
            let reclaim = self.coordinator.reclaim_pending(deadline).await?;
            outcome.reclaim_failures = u64::from(reclaim.failed > 0);
            outcome.needs_retry = reclaim.failed > 0;
        }
        Ok(outcome)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RetentionSchedulerTick {
    pub daily_runs: u64,
    pub daily_failures: u64,
    pub reclaim_runs: u64,
    pub reclaim_failures: u64,
    pub needs_retry: bool,
}

pub(crate) fn system_wall_time() -> Result<RetentionWallTime, PortError> {
    let observed_at = SystemTime::now();
    let millis = observed_at
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| PortError::new(PortErrorClass::InvalidInput, "retention.local_time"))?;
    let timestamp = jiff::Timestamp::from_millisecond(millis)
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "retention.local_time"))?;
    let timezone = jiff::tz::TimeZone::try_system()
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "retention.local_time"))?;
    let local = timestamp.to_zoned(timezone);
    let date = time::Date::from_calendar_date(
        i32::from(local.year()),
        time::Month::try_from(
            u8::try_from(local.month())
                .map_err(|_| PortError::new(PortErrorClass::CorruptData, "retention.local_time"))?,
        )
        .map_err(|_| PortError::new(PortErrorClass::CorruptData, "retention.local_time"))?,
        u8::try_from(local.day())
            .map_err(|_| PortError::new(PortErrorClass::CorruptData, "retention.local_time"))?,
    )
    .map_err(|_| PortError::new(PortErrorClass::CorruptData, "retention.local_time"))?;
    let local_day = date
        .to_julian_day()
        .checked_sub(super::detail_shards::UNIX_EPOCH_JULIAN_DAY)
        .ok_or_else(|| PortError::new(PortErrorClass::InvalidInput, "retention.local_time"))?;
    let hour = u32::try_from(local.hour())
        .map_err(|_| PortError::new(PortErrorClass::CorruptData, "retention.local_time"))?;
    let minute = u32::try_from(local.minute())
        .map_err(|_| PortError::new(PortErrorClass::CorruptData, "retention.local_time"))?;
    let second = u32::try_from(local.second())
        .map_err(|_| PortError::new(PortErrorClass::CorruptData, "retention.local_time"))?;
    let local_second = hour * 3_600 + minute * 60 + second;
    let utc_day = super::statistics::day_utc(observed_at)
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "retention.local_time"))?;
    Ok(RetentionWallTime {
        observed_at,
        local_day,
        local_second,
        utc_day,
    })
}

/// 以当前系统时区计算严格晚于观测时刻的下一次本地 01:00，DST 间隙按 jiff compatible 规则处理。
pub(crate) fn next_scheduled_at_utc_millis(observed_at: SystemTime) -> Result<u64, PortError> {
    let millis = observed_at
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| PortError::new(PortErrorClass::InvalidInput, "retention.next_schedule"))?;
    let timestamp = jiff::Timestamp::from_millisecond(millis)
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "retention.next_schedule"))?;
    let timezone = jiff::tz::TimeZone::try_system()
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "retention.next_schedule"))?;
    next_scheduled_timestamp(timestamp, timezone).and_then(|value| {
        u64::try_from(value.as_millisecond())
            .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "retention.next_schedule"))
    })
}

fn next_scheduled_timestamp(
    timestamp: jiff::Timestamp,
    timezone: jiff::tz::TimeZone,
) -> Result<jiff::Timestamp, PortError> {
    let local = timestamp.to_zoned(timezone.clone());
    let date = jiff::civil::date(local.year(), local.month(), local.day());
    let mut scheduled = date
        .at(1, 0, 0, 0)
        .to_zoned(timezone.clone())
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "retention.next_schedule"))?;
    if scheduled.timestamp() <= timestamp {
        scheduled = date
            .tomorrow()
            .and_then(|date| date.at(1, 0, 0, 0).to_zoned(timezone))
            .map_err(|_| PortError::new(PortErrorClass::Unavailable, "retention.next_schedule"))?;
    }
    Ok(scheduled.timestamp())
}

fn available_range(days: impl Iterator<Item = i32>) -> RetentionAvailableRange {
    days.fold(RetentionAvailableRange::default(), |mut range, day| {
        range.from_day_utc = Some(range.from_day_utc.map_or(day, |current| current.min(day)));
        range.to_day_utc = Some(range.to_day_utc.map_or(day, |current| current.max(day)));
        range
    })
}

fn daily_run_is_due(last_success_local_day: Option<i32>, wall: RetentionWallTime) -> bool {
    wall.local_second >= RETENTION_SCHEDULE_LOCAL_SECOND
        && last_success_local_day.is_none_or(|last| last < wall.local_day)
}

fn reclaim_error_code(error: &PortError) -> &'static str {
    match error.operation() {
        "detail_shard.checkpoint" => "checkpoint",
        "detail_shard.reclaim_close" => "close",
        "detail_shard.reclaim_delete" => "delete",
        "detail_shard.reclaim_open" => "open",
        "detail_shard.path" | "detail_shard.initialize" => "path",
        _ => error.class().as_str(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use crate::dns::{Deadline, RuntimeRevision, TransportClass};
    use crate::ports::observation::ClientMatchSource;
    use crate::ports::storage::{ResolveEvent, StatsSource, StorageBackend};
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::storage::sqlite::InjectedSqliteFault;
    use crate::storage::{
        DetailPageDirection, DetailQuery, DetailQueryFilter, DetailQuerySort, DetailShardStore,
        DetailSortOrder, ResolveDetailRecord, SqliteStorageBackend, StatsPersistenceWorker,
    };

    use super::{
        RETENTION_SCHEDULE_LOCAL_SECOND, RetentionCoordinator, RetentionManifestState,
        RetentionPlan, RetentionPolicy, RetentionPolicyError, RetentionScheduler,
        RetentionWallTime, daily_run_is_due, next_scheduled_timestamp,
    };

    const DAY_MILLIS: u64 = 86_400_000;
    const REFERENCE_DAY: i32 = 20_710;
    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn test_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns")
            .join("p2-retention-tests")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn detail_record(day_utc: i32, qname: &str) -> ResolveDetailRecord {
        ResolveDetailRecord::from_event(ResolveEvent {
            occurred_at: UNIX_EPOCH
                + Duration::from_millis(u64::try_from(day_utc).unwrap() * DAY_MILLIS + 1),
            duration_millis: 2,
            dns_core_duration_micros: 800,
            request_digest: Arc::from("retention-test-digest"),
            listener_id: Arc::from("udp-main"),
            route_id: None,
            client_id: Some(Arc::from("raw-client")),
            client_ip: Some("192.0.2.10".parse().unwrap()),
            client_match_source: Some(ClientMatchSource::Id),
            matched_client_id: Some(Arc::from("client-a")),
            client_bucket: Some(Arc::from("client-a")),
            strategy_id: Some(Arc::from("default")),
            upstream_id: Some(Arc::from("public")),
            upstream_member_id: None,
            upstream_used_id: Some(Arc::from("alidns")),
            matched_rule_source: None,
            matched_resource_id: None,
            matched_rule_ordinal: None,
            resource_version: None,
            transport: TransportClass::Datagram,
            qname: Arc::from(qname),
            qtype: 1,
            qclass: 1,
            answers: Vec::new(),
            rcode: 0,
            cancellation_reason: None,
            outcome: OutcomeClass::Success,
            source: StatsSource::Upstream,
            cache_status: CacheStatus::Miss,
            runtime_revision: RuntimeRevision(1),
        })
        .unwrap()
    }

    fn query(day_utc: i32) -> DetailQuery {
        DetailQuery {
            filter: DetailQueryFilter {
                from_utc_millis: u64::try_from(day_utc).unwrap() * DAY_MILLIS,
                to_utc_millis: u64::try_from(day_utc + 1).unwrap() * DAY_MILLIS,
                ..DetailQueryFilter::default()
            },
            cursor: None,
            direction: DetailPageDirection::Older,
            page_size: 20,
            sort: DetailQuerySort::OccurredAt,
            order: DetailSortOrder::Asc,
        }
    }

    #[test]
    fn calculation_uses_grace_at_threshold_and_includes_reference_day() {
        for (days, grace, size, threshold, expected_target, expected_keep) in [
            (1, 0, 0, 1, 1, REFERENCE_DAY),
            (3, 2, 9, 10, 5, REFERENCE_DAY - 4),
            (7, 3, 10, 10, 10, REFERENCE_DAY - 9),
            (7, 3, 11, 10, 7, REFERENCE_DAY - 6),
            (30, 0, 1 << 40, 1, 30, REFERENCE_DAY - 29),
        ] {
            let policy = RetentionPolicy::new(days, grace, threshold).unwrap();
            let plan = RetentionPlan::calculate(policy, REFERENCE_DAY, size).unwrap();
            assert_eq!(plan.target_days, expected_target);
            assert_eq!(plan.keep_from_day_utc, expected_keep);
        }
        for invalid in [
            RetentionPolicy::new(0, 0, 1),
            RetentionPolicy::new(3_650, 1, 1),
            RetentionPolicy::new(1, 0, 0),
            RetentionPolicy::new(1, 0, (1_u64 << 40) + 1),
        ] {
            assert_eq!(invalid.unwrap_err(), RetentionPolicyError::Invalid);
        }
        assert_eq!(
            RetentionPlan::calculate(RetentionPolicy::default(), i32::MIN, 0).unwrap_err(),
            RetentionPolicyError::DayOutOfRange
        );
        assert_eq!(
            RetentionPlan::calculate(RetentionPolicy::default(), REFERENCE_DAY, u64::MAX)
                .unwrap_err(),
            RetentionPolicyError::SampleTooLarge
        );
    }

    #[test]
    fn next_schedule_is_strictly_after_observation_at_local_one_o_clock() {
        let timezone = jiff::tz::TimeZone::UTC;
        let before = jiff::civil::date(2026, 9, 8)
            .at(0, 59, 59, 0)
            .to_zoned(timezone.clone())
            .unwrap()
            .timestamp();
        let at = jiff::civil::date(2026, 9, 8)
            .at(1, 0, 0, 0)
            .to_zoned(timezone.clone())
            .unwrap()
            .timestamp();
        let today = jiff::civil::date(2026, 9, 8)
            .at(1, 0, 0, 0)
            .to_zoned(timezone.clone())
            .unwrap()
            .timestamp();
        let tomorrow = jiff::civil::date(2026, 9, 9)
            .at(1, 0, 0, 0)
            .to_zoned(timezone.clone())
            .unwrap()
            .timestamp();

        assert_eq!(
            next_scheduled_timestamp(before, timezone.clone()).unwrap(),
            today
        );
        assert_eq!(next_scheduled_timestamp(at, timezone).unwrap(), tomorrow);
    }

    #[tokio::test]
    async fn sample_counts_only_managed_main_and_wal_files() {
        let root = test_root("sample");
        let store = DetailShardStore::new(root.clone(), Vec::new(), 1).unwrap();
        store
            .write_records(
                REFERENCE_DAY,
                &[detail_record(REFERENCE_DAY, "sample.example.")],
                deadline(),
            )
            .await
            .unwrap();
        let main = store.shard_path(REFERENCE_DAY).unwrap();
        let main_bytes = std::fs::metadata(&main).unwrap().len();
        std::fs::write(format!("{}-wal", main.display()), b"wal").unwrap();
        std::fs::write(format!("{}-shm", main.display()), b"ignored-shm").unwrap();
        std::fs::write(root.join("backup.sqlite3.bak"), b"ignored-backup").unwrap();
        let sample = store.sample_managed_storage(deadline()).unwrap();
        assert_eq!(sample.bytes, main_bytes + 3);
        assert_eq!(sample.shard_days, [REFERENCE_DAY]);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn real_sqlite_publication_is_monotonic_and_guards_replay_and_late_writes() {
        let root = test_root("publish");
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("stats.sqlite3");
        let details = root.join("queries");
        let backend = Arc::new(
            SqliteStorageBackend::connect_with_deadline(database.clone(), deadline())
                .await
                .unwrap(),
        );
        backend
            .migrate(crate::storage::STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let bootstrap = backend.retention_bootstrap(deadline()).await.unwrap();
        assert!(bootstrap.state.is_none());
        assert_eq!(bootstrap.next_stats_batch_id, 1);
        let stats = Arc::new(StatsPersistenceWorker::with_next_batch_id(
            backend.clone(),
            bootstrap.next_stats_batch_id,
        ));
        let detail =
            Arc::new(DetailShardStore::new(details.clone(), vec![database.clone()], 2).unwrap());
        for day in [REFERENCE_DAY - 3, REFERENCE_DAY - 2, REFERENCE_DAY] {
            detail
                .write_records(day, &[detail_record(day, "kept.example.")], deadline())
                .await
                .unwrap();
            stats.record_request(day, Vec::new()).unwrap();
        }
        assert_eq!(stats.flush(deadline()).await.unwrap().events_committed, 3);
        // 先制造真实 pending batch；水位发布后重放只能确认 ledger，不能恢复业务统计。
        stats.record_request(REFERENCE_DAY - 4, Vec::new()).unwrap();
        backend.inject_fault(InjectedSqliteFault::Busy);
        assert!(stats.flush(deadline()).await.is_err());
        assert_eq!(stats.pending_batch_count(), 1);

        let coordinator = Arc::new(RetentionCoordinator::new(
            backend.clone(),
            stats.clone(),
            detail.clone(),
        ));
        let policy = RetentionPolicy::new(3, 0, 1 << 40).unwrap();
        let plan = coordinator
            .preview(policy, REFERENCE_DAY, deadline())
            .await
            .unwrap();
        assert_eq!(plan.keep_from_day_utc, REFERENCE_DAY - 2);
        let held_lease = detail
            .acquire_read(REFERENCE_DAY - 3, deadline())
            .await
            .unwrap()
            .unwrap();
        let publish_coordinator = Arc::clone(&coordinator);
        let publication =
            tokio::spawn(async move { publish_coordinator.publish(plan, deadline()).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!publication.is_finished());
        held_lease.close(deadline()).await.unwrap();
        let state = publication.await.unwrap().unwrap();
        assert_eq!(state.revision, 1);
        assert_eq!(state.watermark_revision, 1);
        assert_eq!(state.retired_before_day_utc, REFERENCE_DAY - 2);

        let verification = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&database)
                    .read_only(true),
            )
            .await
            .unwrap();
        let totals: Vec<i64> =
            sqlx::query_scalar("SELECT day_utc FROM stats_daily_total ORDER BY day_utc")
                .fetch_all(&verification)
                .await
                .unwrap();
        assert_eq!(
            totals,
            [i64::from(REFERENCE_DAY - 2), i64::from(REFERENCE_DAY)]
        );
        let manifest: Vec<(i64, String)> =
            sqlx::query_as("SELECT day_utc, state FROM retention_detail_manifest ORDER BY day_utc")
                .fetch_all(&verification)
                .await
                .unwrap();
        assert_eq!(manifest, [(i64::from(REFERENCE_DAY - 3), "pending".into())]);
        let ledger_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stats_batch_ledger")
            .fetch_one(&verification)
            .await
            .unwrap();
        assert_eq!(ledger_count, 0, "confirmed replay prefix must be reclaimed");
        assert!(
            detail
                .query_details(query(REFERENCE_DAY - 3), deadline())
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let dropped = detail
            .write_records(
                REFERENCE_DAY - 3,
                &[detail_record(REFERENCE_DAY - 3, "late.example.")],
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(dropped.dropped, 1);
        assert_eq!(stats.flush(deadline()).await.unwrap().events_committed, 1);
        assert_eq!(stats.pending_batch_count(), 0);
        let retired_total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM stats_daily_total WHERE day_utc < ?")
                .bind(i64::from(REFERENCE_DAY - 2))
                .fetch_one(&verification)
                .await
                .unwrap();
        assert_eq!(retired_total, 0);

        let expanded = RetentionPlan::calculate(
            RetentionPolicy::new(30, 0, 1 << 40).unwrap(),
            REFERENCE_DAY,
            state.sampled_detail_bytes,
        )
        .unwrap();
        let expanded_state = coordinator.publish(expanded, deadline()).await.unwrap();
        assert_eq!(expanded_state.revision, 2);
        assert_eq!(expanded_state.watermark_revision, 1);
        assert_eq!(
            expanded_state.retired_before_day_utc,
            state.retired_before_day_utc
        );
        verification.close().await;
        detail.shutdown(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();
        drop((coordinator, stats, detail, backend));

        let reopened = Arc::new(
            SqliteStorageBackend::connect_with_deadline(database.clone(), deadline())
                .await
                .unwrap(),
        );
        let bootstrap = reopened.retention_bootstrap(deadline()).await.unwrap();
        assert_eq!(
            bootstrap.state.unwrap().retired_before_day_utc,
            REFERENCE_DAY - 2
        );
        assert_eq!(bootstrap.next_stats_batch_id, 3);
        let reopened_stats = StatsPersistenceWorker::with_next_batch_id(
            reopened.clone(),
            bootstrap.next_stats_batch_id,
        );
        reopened_stats
            .record_request(REFERENCE_DAY - 5, Vec::new())
            .unwrap();
        reopened_stats.flush(deadline()).await.unwrap();
        let reopened_verification = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&database)
                    .read_only(true),
            )
            .await
            .unwrap();
        let batch_id: i64 = sqlx::query_scalar("SELECT MAX(batch_id) FROM stats_batch_ledger")
            .fetch_one(&reopened_verification)
            .await
            .unwrap();
        assert_eq!(batch_id, 3);
        reopened_verification.close().await;
        reopened.shutdown(deadline()).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_stats_transaction_does_not_publish_detail_watermark() {
        let root = test_root("rollback");
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("stats.sqlite3");
        let backend = Arc::new(
            SqliteStorageBackend::connect_with_deadline(database.clone(), deadline())
                .await
                .unwrap(),
        );
        let bootstrap = backend.retention_bootstrap(deadline()).await.unwrap();
        let stats = Arc::new(StatsPersistenceWorker::with_next_batch_id(
            backend.clone(),
            bootstrap.next_stats_batch_id,
        ));
        let detail = Arc::new(
            DetailShardStore::new(root.join("queries"), vec![database.clone()], 1).unwrap(),
        );
        let old_day = REFERENCE_DAY - 5;
        detail
            .write_records(
                old_day,
                &[detail_record(old_day, "old.example.")],
                deadline(),
            )
            .await
            .unwrap();
        stats.record_request(old_day, Vec::new()).unwrap();
        stats.flush(deadline()).await.unwrap();
        let sabotage = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&database))
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_stats_retention BEFORE DELETE ON stats_daily_total \
             BEGIN SELECT RAISE(ABORT, 'reject retention'); END",
        )
        .execute(&sabotage)
        .await
        .unwrap();
        let coordinator = RetentionCoordinator::new(backend.clone(), stats.clone(), detail.clone());
        let plan = RetentionPlan::calculate(
            RetentionPolicy::new(1, 0, 1 << 40).unwrap(),
            REFERENCE_DAY,
            0,
        )
        .unwrap();
        assert!(coordinator.publish(plan, deadline()).await.is_err());
        assert_eq!(detail.retired_before(), None);
        assert_eq!(
            detail
                .query_details(query(old_day), deadline())
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        let state_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retention_state")
            .fetch_one(&sabotage)
            .await
            .unwrap();
        assert_eq!(state_count, 0);
        sqlx::query("DROP TRIGGER reject_stats_retention")
            .execute(&sabotage)
            .await
            .unwrap();
        sabotage.close().await;
        detail.shutdown(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();
        drop((coordinator, stats, detail, backend));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wall_clock_due_rule_handles_dst_repeat_rollback_and_timezone_change() {
        let due = |last_success, local_day, local_second| {
            daily_run_is_due(
                last_success,
                RetentionWallTime {
                    observed_at: UNIX_EPOCH,
                    local_day,
                    local_second,
                    utc_day: REFERENCE_DAY,
                },
            )
        };
        assert!(!due(Some(100), 101, RETENTION_SCHEDULE_LOCAL_SECOND - 1));
        // 跳过 01:00 的 DST 场景在首次观测到 02:00 后补跑。
        assert!(due(Some(100), 101, 2 * 60 * 60));
        // 同一本地日重复出现 01:00，以及墙钟回拨到更早日期，都不重复执行。
        assert!(!due(Some(101), 101, RETENTION_SCHEDULE_LOCAL_SECOND));
        assert!(!due(Some(101), 100, 23 * 60 * 60));
        // 运行中切换时区导致本地日期前进时，仍按新本地日执行一次。
        assert!(due(Some(101), 102, RETENTION_SCHEDULE_LOCAL_SECOND));
    }

    #[tokio::test]
    async fn scheduler_retries_failed_day_and_persists_single_run_across_restart() {
        let root = test_root("scheduler");
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("stats.sqlite3");
        let details = root.join("queries");
        let backend = Arc::new(SqliteStorageBackend::connect(&database).await.unwrap());
        let bootstrap = backend.retention_bootstrap(deadline()).await.unwrap();
        let stats = Arc::new(StatsPersistenceWorker::with_next_batch_id(
            backend.clone(),
            bootstrap.next_stats_batch_id,
        ));
        let detail =
            Arc::new(DetailShardStore::new(details.clone(), vec![database.clone()], 1).unwrap());
        let coordinator = Arc::new(RetentionCoordinator::new(
            backend.clone(),
            stats.clone(),
            detail.clone(),
        ));
        let scheduler = RetentionScheduler::new(coordinator.clone(), RetentionPolicy::default());
        let wall = |local_day, local_second, utc_day| RetentionWallTime {
            observed_at: UNIX_EPOCH + Duration::from_secs(u64::try_from(utc_day).unwrap() * 86_400),
            local_day,
            local_second,
            utc_day,
        };

        // 01:00 前启动的新空库以昨日为基线，并在当天首次到达 01:00 时执行。
        let baseline = scheduler
            .tick(
                wall(100, RETENTION_SCHEDULE_LOCAL_SECOND - 1, REFERENCE_DAY),
                true,
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(baseline.daily_runs, 0);
        assert_eq!(
            backend
                .retention_run_state(deadline())
                .await
                .unwrap()
                .last_success_local_day,
            Some(99)
        );
        let first_run = scheduler
            .tick(
                wall(100, RETENTION_SCHEDULE_LOCAL_SECOND, REFERENCE_DAY),
                true,
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(first_run.daily_runs, 1);

        let sabotage = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&database))
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_retention_state BEFORE UPDATE ON retention_state \
             BEGIN SELECT RAISE(ABORT, 'reject retention state'); END",
        )
        .execute(&sabotage)
        .await
        .unwrap();
        let failed = scheduler
            .tick(wall(101, 2 * 60 * 60, REFERENCE_DAY + 1), true, deadline())
            .await
            .unwrap();
        assert_eq!((failed.daily_runs, failed.daily_failures), (1, 1));
        assert!(failed.needs_retry);
        assert_eq!(
            backend
                .retention_run_state(deadline())
                .await
                .unwrap()
                .consecutive_failures,
            1
        );
        let gated = scheduler
            .tick(
                wall(101, RETENTION_SCHEDULE_LOCAL_SECOND, REFERENCE_DAY + 1),
                false,
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(gated.daily_runs, 0);
        sqlx::query("DROP TRIGGER reject_retention_state")
            .execute(&sabotage)
            .await
            .unwrap();
        sabotage.close().await;
        let recovered = scheduler
            .tick(
                wall(101, RETENTION_SCHEDULE_LOCAL_SECOND, REFERENCE_DAY + 1),
                true,
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!((recovered.daily_runs, recovered.daily_failures), (1, 0));
        assert_eq!(
            backend
                .retention_run_state(deadline())
                .await
                .unwrap()
                .last_success_local_day,
            Some(101)
        );
        detail.shutdown(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();
        drop((scheduler, coordinator, detail, stats, backend));

        let reopened = Arc::new(SqliteStorageBackend::connect(&database).await.unwrap());
        let bootstrap = reopened.retention_bootstrap(deadline()).await.unwrap();
        let reopened_stats = Arc::new(StatsPersistenceWorker::with_next_batch_id(
            reopened.clone(),
            bootstrap.next_stats_batch_id,
        ));
        let reopened_detail =
            Arc::new(DetailShardStore::new(details, vec![database.clone()], 1).unwrap());
        let reopened_coordinator = Arc::new(RetentionCoordinator::new(
            reopened.clone(),
            reopened_stats.clone(),
            reopened_detail.clone(),
        ));
        let reopened_scheduler =
            RetentionScheduler::new(reopened_coordinator.clone(), RetentionPolicy::default());
        let repeated = reopened_scheduler
            .tick(wall(101, 2 * 60 * 60, REFERENCE_DAY + 1), true, deadline())
            .await
            .unwrap();
        assert_eq!(repeated.daily_runs, 0);
        let next_day = reopened_scheduler
            .tick(wall(102, 2 * 60 * 60, REFERENCE_DAY + 2), true, deadline())
            .await
            .unwrap();
        assert_eq!(next_day.daily_runs, 1);
        reopened_detail.shutdown(deadline()).await.unwrap();
        reopened.shutdown(deadline()).await.unwrap();
        drop((reopened_scheduler, reopened_coordinator, reopened_stats));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn real_reclaim_tracks_failure_retries_files_and_leaves_cache_untouched() {
        let root = test_root("reclaim");
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("stats.sqlite3");
        let details = root.join("queries");
        let cache = root.join("dns-cache.fdcs");
        std::fs::write(&cache, b"cache-sentinel").unwrap();
        let backend = Arc::new(SqliteStorageBackend::connect(&database).await.unwrap());
        let bootstrap = backend.retention_bootstrap(deadline()).await.unwrap();
        let stats = Arc::new(StatsPersistenceWorker::with_next_batch_id(
            backend.clone(),
            bootstrap.next_stats_batch_id,
        ));
        let detail = Arc::new(
            DetailShardStore::new(details, vec![database.clone(), cache.clone()], 2).unwrap(),
        );
        let old_day = REFERENCE_DAY - 3;
        for day in [old_day, REFERENCE_DAY - 1, REFERENCE_DAY] {
            detail
                .write_records(day, &[detail_record(day, "reclaim.example.")], deadline())
                .await
                .unwrap();
            stats.record_request(day, Vec::new()).unwrap();
        }
        stats.flush(deadline()).await.unwrap();
        let coordinator = RetentionCoordinator::new(backend.clone(), stats.clone(), detail.clone());
        let state = coordinator
            .run_daily(
                RetentionPolicy::new(2, 0, 1 << 40).unwrap(),
                REFERENCE_DAY,
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(state.retired_before_day_utc, REFERENCE_DAY - 1);
        let old_path = detail.shard_path(old_day).unwrap();
        detail.fail_next_reclaim_delete_for_test();
        let failed = coordinator.reclaim_pending(deadline()).await.unwrap();
        assert_eq!(
            (failed.attempted, failed.reclaimed, failed.failed),
            (1, 0, 1)
        );
        assert!(old_path.exists());
        let entries = backend
            .pending_retention_reclaims(deadline())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, RetentionManifestState::Failed);
        assert_eq!(entries[0].attempts, 1);
        assert_eq!(entries[0].last_error_code.as_deref(), Some("delete"));
        let pending = coordinator
            .status(
                RetentionPolicy::new(2, 0, 1 << 40).unwrap(),
                REFERENCE_DAY,
                deadline(),
            )
            .await
            .unwrap();
        assert!(pending.pending_reclaim_bytes > 0);

        let recovered = coordinator.reclaim_pending(deadline()).await.unwrap();
        assert_eq!(
            (recovered.attempted, recovered.reclaimed, recovered.failed),
            (1, 1, 0)
        );
        assert!(!old_path.exists());
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(!PathBuf::from(format!("{}{}", old_path.display(), suffix)).exists());
        }
        assert_eq!(std::fs::read(&cache).unwrap(), b"cache-sentinel");

        let status = coordinator
            .status(
                RetentionPolicy::new(2, 0, 1 << 40).unwrap(),
                REFERENCE_DAY,
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(status.target_days, 2);
        assert_eq!(
            status.next_expected_retired_before_day_utc,
            REFERENCE_DAY - 1
        );
        assert_eq!(
            status.detail_available.from_day_utc,
            Some(REFERENCE_DAY - 1)
        );
        assert_eq!(status.detail_available.to_day_utc, Some(REFERENCE_DAY));
        assert_eq!(status.stats_available.from_day_utc, Some(REFERENCE_DAY - 1));
        assert_eq!(status.stats_available.to_day_utc, Some(REFERENCE_DAY));
        assert_eq!((status.pending_reclaims, status.failed_reclaims), (0, 0));
        assert_eq!(status.pending_reclaim_bytes, 0);
        assert!(status.last_cleanup_at.is_some());

        let verification = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&database)
                    .read_only(true),
            )
            .await
            .unwrap();
        let manifest: (String, i64, Option<String>) = sqlx::query_as(
            "SELECT state, attempts, last_error_code FROM retention_detail_manifest WHERE day_utc = ?",
        )
        .bind(i64::from(old_day))
        .fetch_one(&verification)
        .await
        .unwrap();
        assert_eq!(manifest, ("reclaimed".into(), 2, None));
        verification.close().await;
        detail.shutdown(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();
        drop((coordinator, stats, detail, backend));
        std::fs::remove_dir_all(root).unwrap();
    }
}
