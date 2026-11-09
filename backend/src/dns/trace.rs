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

/// 内存聚合与持久化 execution 分离，避免新增字段破坏旧版本严格 JSON 读取。
#[derive(Clone, Debug, Default)]
pub(crate) struct RequestObservationSnapshot {
    pub execution: RequestTraceSnapshot,
    pub ecs: Option<super::ecs::QueryEcs>,
}

/// 默认不采集 ECS，统一入口仅在详情启用时打开；刷新 clone 只拥有刷新槽。
#[derive(Clone, Debug, Default)]
pub struct RequestTrace(
    Option<watch::Sender<RequestObservationSnapshot>>,
    Option<Instant>,
    bool,
);

impl RequestTrace {
    /// 起点为 transport 收到完整请求，不包含连接空闲或后台刷新时间。
    pub fn new(received_at: Instant) -> Self {
        let (sender, _) = watch::channel(RequestObservationSnapshot {
            execution: RequestTraceSnapshot {
                response_status: ResponseDelivery::Pending,
                ..Default::default()
            },
            ecs: None,
        });
        Self(Some(sender), Some(received_at), false)
    }

    /// 只在原始 query 边界开启，不用配置或客户端地址补算原始输入。
    pub(crate) fn observe_request(&self, query: &super::CanonicalQuery) {
        if let Some(sender) = &self.0 {
            sender.send_if_modified(|value| {
                if value.ecs.is_some() {
                    return false;
                }
                value.ecs = Some(super::ecs::QueryEcs {
                    request: super::ecs_observation::query_value(query),
                    upstream: super::ecs::EcsExchange::NotSent {
                        reason: super::ecs::EcsNotSentReason::NotStarted,
                    },
                    cache_match: None,
                    refresh: None,
                });
                true
            });
        }
    }

    pub(crate) fn ecs_enabled(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|sender| sender.borrow().ecs.is_some())
    }

    /// 后台刷新共享生命周期，但永不拥有前台应答 ECS。
    pub(crate) fn for_refresh(&self) -> Self {
        Self(self.0.clone(), self.1, true)
    }

    pub(crate) fn exchange(&self, exchange: super::ecs::EcsExchange) {
        if let Some(sender) = &self.0 {
            sender.send_modify(|value| {
                if let Some(ecs) = &mut value.ecs {
                    if self.2 {
                        ecs.refresh = Some(exchange);
                    } else {
                        ecs.upstream = exchange;
                    }
                }
            });
        }
    }

    pub(crate) fn not_sent(&self, reason: super::ecs::EcsNotSentReason) {
        if self.ecs_enabled() {
            self.exchange(super::ecs::EcsExchange::NotSent { reason });
        }
    }

    pub(crate) fn cache_match(&self, matched: super::ecs::EcsCacheMatch) {
        if let Some(sender) = &self.0 {
            sender.send_modify(|value| {
                if let Some(ecs) = &mut value.ecs {
                    ecs.cache_match = Some(matched);
                }
            });
        }
    }

    /// 只记录服务端成功写出所花时间；失败、取消不能伪装为已响应。
    pub fn finish_response(&self, status: ResponseDelivery, started: Instant) {
        if let Some(sender) = &self.0 {
            sender.send_modify(|value| {
                let value = &mut value.execution;
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
                value.execution.cache_activity = Some(CacheActivity {
                    kind,
                    outcome: CacheActivityOutcome::Pending,
                    upstream_target_name: None,
                    upstream_used_name: None,
                    response_class: None,
                    ttl_secs: None,
                });
                if kind == CacheActivityKind::Refresh
                    && let Some(ecs) = &mut value.ecs
                {
                    ecs.refresh = Some(super::ecs::EcsExchange::NotSent {
                        reason: super::ecs::EcsNotSentReason::NotStarted,
                    });
                }
            });
        }
        CacheActivityGuard {
            trace: self.clone(),
            finished: false,
        }
    }

    /// 兼容 execution-only 消费者，不将 ECS 混入旧 JSON。
    pub async fn settled(&self) -> RequestTraceSnapshot {
        self.settled_observation().await.execution
    }

    /// 同一 watch 快照同时冻结两份详情；预算仍为六秒，不新增主链等待。
    pub(crate) async fn settled_observation(&self) -> RequestObservationSnapshot {
        let Some(sender) = &self.0 else {
            return RequestObservationSnapshot::default();
        };
        let mut receiver = sender.subscribe();
        let wait = async {
            loop {
                let snapshot = receiver.borrow_and_update().clone();
                if snapshot.execution.response_status != ResponseDelivery::Pending
                    && !snapshot
                        .execution
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
                if snapshot.execution.response_status == ResponseDelivery::Pending {
                    snapshot.execution.response_status = ResponseDelivery::Unrecorded;
                }
                if let Some(activity) = &mut snapshot.execution.cache_activity
                    && activity.outcome == CacheActivityOutcome::Pending
                {
                    activity.outcome = CacheActivityOutcome::Unrecorded;
                    if activity.kind == CacheActivityKind::Refresh
                        && let Some(ecs) = &mut snapshot.ecs
                        && !matches!(ecs.refresh, Some(super::ecs::EcsExchange::Observed { .. }))
                    {
                        ecs.refresh = Some(super::ecs::EcsExchange::Unrecorded {
                            reason: super::ecs::EcsUnrecordedReason::Deadline,
                        });
                    }
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
                if let Some(activity) = &mut value.execution.cache_activity {
                    activity.upstream_target_name = target.map(str::to_owned);
                    activity.upstream_used_name = used.map(str::to_owned);
                }
            });
        }
    }

    /// 记录本次取得的响应分类与实际写入条目的生命周期。
    pub fn result(&self, class: CacheActivityResponseClass, ttl_secs: Option<u64>) {
        if let Some(sender) = &self.trace.0 {
            sender.send_modify(|value| {
                if let Some(activity) = &mut value.execution.cache_activity {
                    activity.response_class = Some(class);
                    activity.ttl_secs = ttl_secs;
                }
            });
        }
    }

    /// 事实必须在终态前发布，使 projector 读取同一时刻的一致快照。
    pub fn finish(mut self, outcome: CacheActivityOutcome) {
        self.set_outcome(outcome);
        self.finished = true;
    }

    fn set_outcome(&self, outcome: CacheActivityOutcome) {
        if let Some(sender) = &self.trace.0 {
            sender.send_modify(|value| {
                if let Some(activity) = &mut value.execution.cache_activity {
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
            .execution
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

    fn ecs_fixture() -> super::super::ecs::QueryEcs {
        use super::super::ecs::*;
        QueryEcs {
            request: EcsValue::Present {
                cidr: "203.0.113.99/32".into(),
                scope_prefix: 0,
            },
            upstream: EcsExchange::NotSent {
                reason: EcsNotSentReason::CacheHit,
            },
            cache_match: Some(EcsCacheMatch {
                kind: EcsCacheMatchKind::Exact,
                cidr: Some("0.0.0.0/0".into()),
            }),
            refresh: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ecs_deadline_preserves_known_request_and_cache_without_extending_wait() {
        use super::super::ecs::*;
        let trace = RequestTrace::new(Instant::now());
        trace
            .0
            .as_ref()
            .unwrap()
            .send_modify(|value| value.ecs = Some(ecs_fixture()));
        trace.finish_response(ResponseDelivery::Sent, Instant::now());
        let _refresh = trace.begin_cache(CacheActivityKind::Refresh);
        let snapshot = trace.settled_observation().await;
        let ecs = snapshot.ecs.unwrap();
        assert_eq!(ecs.request, ecs_fixture().request);
        assert_eq!(ecs.cache_match, ecs_fixture().cache_match);
        assert_eq!(ecs.upstream, ecs_fixture().upstream);
        assert_eq!(
            ecs.refresh,
            Some(EcsExchange::Unrecorded {
                reason: EcsUnrecordedReason::Deadline
            })
        );
        let json = serde_json::to_value(snapshot.execution).unwrap();
        assert_eq!(
            json.as_object().unwrap().len(),
            3,
            "execution_json 字段集合保持不变"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn observed_refresh_survives_pending_cache_write_budget_and_frozen_snapshot() {
        use super::super::ecs::*;
        let trace = RequestTrace::new(Instant::now());
        trace
            .0
            .as_ref()
            .unwrap()
            .send_modify(|value| value.ecs = Some(ecs_fixture()));
        trace.finish_response(ResponseDelivery::Sent, Instant::now());
        let _refresh = trace.begin_cache(CacheActivityKind::Refresh);
        let exchange = EcsExchange::Observed {
            query: EcsValue::Absent,
            response: EcsValue::Absent,
            policy: EcsPolicy {
                source: EcsPolicySource::Global,
                mode: EcsPolicyMode::Disabled,
                value_source: EcsValueSource::None,
            },
            upstream_leaf_name: "leaf".into(),
        };
        trace.for_refresh().exchange(exchange.clone());
        let snapshot = trace.settled_observation().await;
        assert_eq!(snapshot.ecs.as_ref().unwrap().refresh, Some(exchange));
        trace.for_refresh().not_sent(EcsNotSentReason::NotStarted);
        assert!(matches!(
            snapshot.ecs.unwrap().refresh,
            Some(EcsExchange::Observed { .. })
        ));
    }
}
