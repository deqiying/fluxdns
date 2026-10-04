//! 仅用于请求详情的响应发送与异步缓存结果关联，不参与响应决策或聚合统计。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseDelivery {
    #[default]
    Unrecorded,
    Pending,
    Sent,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheActivityKind {
    Write,
    Refresh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheActivityOutcome {
    Pending,
    Inserted,
    Updated,
    Rejected,
    Conflict,
    Failed,
    Skipped,
    Coalesced,
    Dropped,
    Unrecorded,
}

/// 缓存写入或刷新实际取得的上游响应分类，用于区分“更新为新记录”与“仍为空应答”。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheActivityResponseClass {
    Positive,
    Nodata,
    Nxdomain,
    Servfail,
    Truncated,
    Refused,
    Other,
}

impl From<super::ResponseClass> for CacheActivityResponseClass {
    fn from(value: super::ResponseClass) -> Self {
        match value {
            super::ResponseClass::Positive => Self::Positive,
            super::ResponseClass::NoData => Self::Nodata,
            super::ResponseClass::NxDomain => Self::Nxdomain,
            super::ResponseClass::ServFail => Self::Servfail,
            super::ResponseClass::Truncated => Self::Truncated,
            super::ResponseClass::Refused => Self::Refused,
            super::ResponseClass::Other(_) => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheActivity {
    pub kind: CacheActivityKind,
    pub outcome: CacheActivityOutcome,
    pub upstream_target_name: Option<String>,
    pub upstream_used_name: Option<String>,
    /// 本次写入/刷新取得的响应分类；未取得响应或历史记录缺失时为空。
    #[serde(default)]
    pub response_class: Option<CacheActivityResponseClass>,
    /// 实际写入条目的缓存生命周期（秒）；未写入时为空。
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestTraceSnapshot {
    pub response_status: ResponseDelivery,
    pub response_duration_us: Option<u64>,
    pub cache_activity: Option<CacheActivity>,
}

/// 默认禁用，实际入站请求使用共享 watch；多个阶段只更新各自拥有的字段。
#[derive(Clone, Debug, Default)]
pub struct RequestTrace(Option<watch::Sender<RequestTraceSnapshot>>, Option<Instant>);

impl RequestTrace {
    /// 起点为 transport 收到完整请求，不包含连接空闲或后台刷新时间。
    pub fn new(received_at: Instant) -> Self {
        let (sender, _) = watch::channel(RequestTraceSnapshot {
            response_status: ResponseDelivery::Pending,
            ..Default::default()
        });
        Self(Some(sender), Some(received_at))
    }

    /// 只记录服务端成功写出所花时间；失败、取消不能伪装为已响应。
    pub fn finish_response(&self, status: ResponseDelivery, started: Instant) {
        if let Some(sender) = &self.0 {
            sender.send_modify(|value| {
                if value.response_status == ResponseDelivery::Pending {
                    value.response_status = status;
                    value.response_duration_us = (status == ResponseDelivery::Sent).then(|| {
                        u64::try_from(self.1.unwrap_or(started).elapsed().as_micros())
                            .unwrap_or(u64::MAX)
                    });
                }
            });
        }
    }

    /// guard 被队列拒绝、任务取消或 panic 丢弃时明确记录 dropped。
    pub fn begin_cache(&self, kind: CacheActivityKind) -> CacheActivityGuard {
        if let Some(sender) = &self.0 {
            sender.send_modify(|value| {
                value.cache_activity = Some(CacheActivity {
                    kind,
                    outcome: CacheActivityOutcome::Pending,
                    upstream_target_name: None,
                    upstream_used_name: None,
                    response_class: None,
                    ttl_secs: None,
                })
            });
        }
        CacheActivityGuard {
            trace: self.clone(),
            finished: false,
        }
    }

    /// 详情等待与客户端响应完全解耦；预算结束显式记为未记录，不永久保留 pending。
    pub async fn settled(&self) -> RequestTraceSnapshot {
        let Some(sender) = &self.0 else {
            return RequestTraceSnapshot::default();
        };
        let mut receiver = sender.subscribe();
        let wait = async {
            loop {
                let snapshot = receiver.borrow_and_update().clone();
                if snapshot.response_status != ResponseDelivery::Pending
                    && !snapshot
                        .cache_activity
                        .as_ref()
                        .is_some_and(|a| a.outcome == CacheActivityOutcome::Pending)
                {
                    return snapshot;
                }
                if receiver.changed().await.is_err() {
                    return snapshot;
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs(6), wait).await {
            Ok(snapshot) => snapshot,
            Err(_) => {
                let mut snapshot = sender.borrow().clone();
                if snapshot.response_status == ResponseDelivery::Pending {
                    snapshot.response_status = ResponseDelivery::Unrecorded;
                }
                if let Some(activity) = &mut snapshot.cache_activity
                    && activity.outcome == CacheActivityOutcome::Pending
                {
                    activity.outcome = CacheActivityOutcome::Unrecorded;
                }
                snapshot
            }
        }
    }
}

#[derive(Debug)]
pub struct CacheActivityGuard {
    trace: RequestTrace,
    finished: bool,
}

impl CacheActivityGuard {
    /// 刷新路由来自本次实际 exchange，与旧缓存生产路由独立。
    pub fn route(&self, target: Option<&str>, used: Option<&str>) {
        if let Some(sender) = &self.trace.0 {
            sender.send_modify(|value| {
                if let Some(activity) = &mut value.cache_activity {
                    activity.upstream_target_name = target.map(str::to_owned);
                    activity.upstream_used_name = used.map(str::to_owned);
                }
            });
        }
    }

    /// 记录本次取得的响应分类与实际写入条目的生命周期，供详情区分刷新结果。
    pub fn result(&self, class: CacheActivityResponseClass, ttl_secs: Option<u64>) {
        if let Some(sender) = &self.trace.0 {
            sender.send_modify(|value| {
                if let Some(activity) = &mut value.cache_activity {
                    activity.response_class = Some(class);
                    activity.ttl_secs = ttl_secs;
                }
            });
        }
    }

    /// 冻结当前缓存任务终态，随后 drop 不再写入 dropped。
    pub fn finish(mut self, outcome: CacheActivityOutcome) {
        self.set_outcome(outcome);
        self.finished = true;
    }

    fn set_outcome(&self, outcome: CacheActivityOutcome) {
        if let Some(sender) = &self.trace.0 {
            sender.send_modify(|value| {
                if let Some(activity) = &mut value.cache_activity {
                    activity.outcome = outcome;
                }
            });
        }
    }
}

impl Drop for CacheActivityGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.set_outcome(CacheActivityOutcome::Dropped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn response_time_freezes_before_refresh_finishes_and_drop_is_explicit() {
        let start = Instant::now() - Duration::from_millis(5);
        let trace = RequestTrace::new(start);
        let refresh = trace.begin_cache(CacheActivityKind::Refresh);
        trace.finish_response(ResponseDelivery::Sent, start);
        let duration = trace
            .0
            .as_ref()
            .unwrap()
            .borrow()
            .response_duration_us
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(refresh);
        let snapshot = trace.settled().await;
        assert_eq!(snapshot.response_duration_us, Some(duration));
        assert!(duration >= 5_000);
        assert_eq!(
            snapshot.cache_activity.unwrap().outcome,
            CacheActivityOutcome::Dropped
        );
        trace.finish_response(ResponseDelivery::Cancelled, start);
        assert_eq!(
            trace.settled().await.response_status,
            ResponseDelivery::Sent
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_wait_does_not_claim_unobserved_success() {
        let trace = RequestTrace::new(Instant::now());
        let _refresh = trace.begin_cache(CacheActivityKind::Refresh);
        let snapshot = trace.settled().await;
        assert_eq!(snapshot.response_status, ResponseDelivery::Unrecorded);
        assert_eq!(snapshot.response_duration_us, None);
        assert_eq!(
            snapshot.cache_activity.unwrap().outcome,
            CacheActivityOutcome::Unrecorded
        );
    }
}
