//! 单资源刷新调度决策。
//!
//! 该模块只维护调用方提供的逻辑时间，不创建 timer、线程或后台任务。实际的
//! refresh worker 应根据 [`ResourceScheduleDecision`] 自行安排 I/O，并在成功、
//! 失败、取消或 shutdown 后提交对应状态转换。

use std::fmt;

use super::RefreshBackoff;

/// 单资源刷新调度的静态策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceSchedulePolicy {
    update_interval: u64,
    stale_after_failures: u32,
    stale_after: u64,
    backoff: RefreshBackoff,
}

impl ResourceSchedulePolicy {
    /// 创建调度策略。
    ///
    /// `stale_after_intervals` 表示距离最近一次成功、或首次排队时间经过多少个
    /// `update_interval` 后进入 stale。两个 stale 阈值均为正数，避免产生永不
    /// stale 的隐含分支。
    pub const fn new(
        update_interval: u64,
        stale_after_intervals: u32,
        stale_after_failures: u32,
        backoff: RefreshBackoff,
    ) -> Option<Self> {
        if update_interval == 0 || stale_after_intervals == 0 || stale_after_failures == 0 {
            return None;
        }
        Some(Self {
            update_interval,
            stale_after_failures,
            stale_after: update_interval.saturating_mul(stale_after_intervals as u64),
            backoff,
        })
    }

    pub const fn update_interval(self) -> u64 {
        self.update_interval
    }

    pub const fn stale_after_failures(self) -> u32 {
        self.stale_after_failures
    }

    pub const fn stale_after(self) -> u64 {
        self.stale_after
    }

    pub const fn backoff(self) -> RefreshBackoff {
        self.backoff
    }
}

/// 调度停止的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceScheduleStopReason {
    Cancelled,
    Shutdown,
}

/// 当前时刻对单资源刷新器的动作建议。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceScheduleDecision {
    /// 当前已经到达刷新截止时间，调用方可以申请一次 refresh reservation。
    Due {
        due_at: u64,
        consecutive_failures: u32,
        stale: bool,
    },
    /// 当前尚未到期；调用方可以等待到 `next_due`，但不应在此之前排队刷新。
    Waiting {
        next_due: u64,
        consecutive_failures: u32,
        stale: bool,
    },
    /// 资源已取消或进程已 shutdown，后续不会再产生刷新截止时间。
    Stopped { reason: ResourceScheduleStopReason },
}

impl ResourceScheduleDecision {
    pub const fn is_due(self) -> bool {
        matches!(self, Self::Due { .. })
    }

    pub const fn is_stopped(self) -> bool {
        matches!(self, Self::Stopped { .. })
    }

    pub const fn next_due(self) -> Option<u64> {
        match self {
            Self::Due { due_at, .. } => Some(due_at),
            Self::Waiting { next_due, .. } => Some(next_due),
            Self::Stopped { .. } => None,
        }
    }

    pub const fn consecutive_failures(self) -> u32 {
        match self {
            Self::Due {
                consecutive_failures,
                ..
            }
            | Self::Waiting {
                consecutive_failures,
                ..
            } => consecutive_failures,
            Self::Stopped { .. } => 0,
        }
    }

    pub const fn is_stale(self) -> bool {
        match self {
            Self::Due { stale, .. } | Self::Waiting { stale, .. } => stale,
            Self::Stopped { .. } => false,
        }
    }
}

/// 单资源刷新调度状态机。
///
/// `new` 会把资源首次刷新安排在 `initial_due`。成功会清零连续失败计数并安排
/// `update_interval` 后的下一次刷新；失败使用 [`RefreshBackoff`] 指数退避。该
/// 类型不保存资源内容、URL、域名或其他敏感字段，因此其 `Debug` 输出不含资源
/// 标识和 payload。
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ResourceSchedule {
    policy: ResourceSchedulePolicy,
    initial_due: u64,
    next_due: Option<u64>,
    last_success_at: Option<u64>,
    consecutive_failures: u32,
    stopped: Option<ResourceScheduleStopReason>,
}

impl ResourceSchedule {
    pub const fn new(policy: ResourceSchedulePolicy, initial_due: u64) -> Self {
        Self {
            policy,
            initial_due,
            next_due: Some(initial_due),
            last_success_at: None,
            consecutive_failures: 0,
            stopped: None,
        }
    }

    pub const fn policy(self) -> ResourceSchedulePolicy {
        self.policy
    }

    pub const fn next_due(self) -> Option<u64> {
        self.next_due
    }

    pub const fn last_success_at(self) -> Option<u64> {
        self.last_success_at
    }

    pub const fn consecutive_failures(self) -> u32 {
        self.consecutive_failures
    }

    pub const fn is_stopped(self) -> bool {
        self.stopped.is_some()
    }

    pub const fn is_stale_at(self, now: u64) -> bool {
        if self.consecutive_failures >= self.policy.stale_after_failures {
            return true;
        }
        let reference = match self.last_success_at {
            Some(last_success_at) => last_success_at,
            None => self.initial_due,
        };
        now.saturating_sub(reference) > self.policy.stale_after
    }

    /// 返回当前时刻的纯逻辑调度决策。
    pub fn decision(self, now: u64) -> ResourceScheduleDecision {
        if let Some(reason) = self.stopped {
            return ResourceScheduleDecision::Stopped { reason };
        }
        let next_due = match self.next_due {
            Some(next_due) => next_due,
            None => panic!("active resource schedule must have a deadline"),
        };
        let stale = self.is_stale_at(now);
        if now >= next_due {
            ResourceScheduleDecision::Due {
                due_at: next_due,
                consecutive_failures: self.consecutive_failures,
                stale,
            }
        } else {
            ResourceScheduleDecision::Waiting {
                next_due,
                consecutive_failures: self.consecutive_failures,
                stale,
            }
        }
    }

    /// 记录一次成功，并在成功时间之后安排固定间隔的下一次刷新。
    pub fn record_success(&mut self, completed_at: u64) -> ResourceScheduleDecision {
        if let Some(reason) = self.stopped {
            return ResourceScheduleDecision::Stopped { reason };
        }
        self.last_success_at = Some(completed_at);
        self.consecutive_failures = 0;
        self.next_due = Some(completed_at.saturating_add(self.policy.update_interval));
        self.decision(completed_at)
    }

    /// 记录一次失败，并按连续失败次数安排指数退避后的下一次刷新。
    pub fn record_failure(&mut self, failed_at: u64) -> ResourceScheduleDecision {
        if let Some(reason) = self.stopped {
            return ResourceScheduleDecision::Stopped { reason };
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let mut shift = self.consecutive_failures.saturating_sub(1);
        if shift > 63 {
            shift = 63;
        }
        let shifted = self.policy.backoff.base_delay().checked_shl(shift);
        let mut delay = shifted.unwrap_or(u64::MAX);
        if delay > self.policy.backoff.max_delay() {
            delay = self.policy.backoff.max_delay();
        }
        self.next_due = Some(failed_at.saturating_add(delay));
        self.decision(failed_at)
    }

    /// 取消当前资源后停止所有后续刷新排队。
    pub fn cancel(&mut self) -> ResourceScheduleDecision {
        self.stop(ResourceScheduleStopReason::Cancelled)
    }

    /// 进程 shutdown 后停止所有后续刷新排队。
    pub fn shutdown(&mut self) -> ResourceScheduleDecision {
        self.stop(ResourceScheduleStopReason::Shutdown)
    }

    fn stop(&mut self, reason: ResourceScheduleStopReason) -> ResourceScheduleDecision {
        if let Some(existing) = self.stopped {
            return ResourceScheduleDecision::Stopped { reason: existing };
        }
        self.stopped = Some(reason);
        self.next_due = None;
        ResourceScheduleDecision::Stopped { reason }
    }
}

impl fmt::Debug for ResourceSchedule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceSchedule")
            .field("policy", &self.policy)
            .field("initial_due", &self.initial_due)
            .field("next_due", &self.next_due)
            .field("last_success_at", &self.last_success_at)
            .field("consecutive_failures", &self.consecutive_failures)
            .field("stopped", &self.stopped)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ResourceSchedulePolicy {
        ResourceSchedulePolicy::new(10, 3, 3, RefreshBackoff::new(2, 8).unwrap())
            .expect("valid schedule policy")
    }

    #[test]
    fn initial_schedule_is_due_at_requested_time() {
        let schedule = ResourceSchedule::new(policy(), 100);
        assert_eq!(
            schedule.decision(99),
            ResourceScheduleDecision::Waiting {
                next_due: 100,
                consecutive_failures: 0,
                stale: false,
            }
        );
        assert_eq!(
            schedule.decision(100),
            ResourceScheduleDecision::Due {
                due_at: 100,
                consecutive_failures: 0,
                stale: false,
            }
        );
    }

    #[test]
    fn success_schedules_next_fixed_deadline_and_resets_failures() {
        let mut schedule = ResourceSchedule::new(policy(), 100);
        schedule.record_failure(100);
        assert_eq!(schedule.consecutive_failures(), 1);
        assert_eq!(
            schedule.record_success(105),
            ResourceScheduleDecision::Waiting {
                next_due: 115,
                consecutive_failures: 0,
                stale: false,
            }
        );
        assert_eq!(schedule.last_success_at(), Some(105));
    }

    #[test]
    fn failure_uses_exponential_backoff_and_caps() {
        let mut schedule = ResourceSchedule::new(policy(), 0);
        assert_eq!(schedule.record_failure(10).next_due(), Some(12));
        assert_eq!(schedule.record_failure(12).next_due(), Some(16));
        assert_eq!(schedule.record_failure(16).next_due(), Some(24));
        assert_eq!(schedule.record_failure(24).next_due(), Some(32));
        assert_eq!(schedule.record_failure(32).next_due(), Some(40));
        assert_eq!(schedule.consecutive_failures(), 5);
    }

    #[test]
    fn stale_is_marked_by_failure_count_or_elapsed_interval() {
        let schedule = ResourceSchedule::new(policy(), 0);
        assert!(!schedule.decision(30).is_stale());
        assert!(schedule.decision(31).is_stale());

        let mut schedule = ResourceSchedule::new(policy(), 0);
        schedule.record_failure(0);
        schedule.record_failure(2);
        assert!(!schedule.decision(6).is_stale());
        assert!(schedule.record_failure(6).is_stale());
    }

    #[test]
    fn cancellation_and_shutdown_remove_future_deadlines() {
        let mut cancelled = ResourceSchedule::new(policy(), 0);
        assert_eq!(
            cancelled.cancel(),
            ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Cancelled,
            }
        );
        assert_eq!(cancelled.next_due(), None);
        assert_eq!(
            cancelled.record_success(10),
            ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Cancelled,
            }
        );
        assert!(cancelled.decision(u64::MAX).is_stopped());

        let mut shutdown = ResourceSchedule::new(policy(), 0);
        assert_eq!(
            shutdown.shutdown(),
            ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Shutdown,
            }
        );
        assert_eq!(shutdown.record_failure(10).next_due(), None);
    }

    #[test]
    fn debug_output_contains_state_but_no_resource_payload() {
        let schedule = ResourceSchedule::new(policy(), 7);
        let debug = format!("{schedule:?}");
        assert!(debug.contains("ResourceSchedule"));
        assert!(debug.contains("consecutive_failures"));
        assert!(!debug.contains("secret.example"));
        assert!(!debug.contains("payload"));
    }
}
