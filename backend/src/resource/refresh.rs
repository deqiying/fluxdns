//! 资源刷新协调：单资源 single-flight、epoch 分配和失败退避。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::config::resolve::ConfigId;

use super::{ResourcePublishError, ResourceRegistrySnapshot, ResourceSnapshot, ResourceVersion};

/// 资源刷新失败后的逻辑时间退避策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefreshBackoff {
    base_delay: u64,
    max_delay: u64,
}

impl RefreshBackoff {
    pub const fn new(base_delay: u64, max_delay: u64) -> Option<Self> {
        if base_delay <= max_delay {
            Some(Self {
                base_delay,
                max_delay,
            })
        } else {
            None
        }
    }

    pub const fn base_delay(self) -> u64 {
        self.base_delay
    }

    pub const fn max_delay(self) -> u64 {
        self.max_delay
    }
}

impl Default for RefreshBackoff {
    fn default() -> Self {
        Self {
            base_delay: 1,
            max_delay: 64,
        }
    }
}

/// 当前资源刷新阶段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceRefreshPhase {
    Idle,
    Refreshing {
        epoch: u64,
    },
    Backoff {
        retry_not_before: u64,
        consecutive_failures: u32,
    },
}

/// 供调度器和管理面读取的资源刷新状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceRefreshStatus {
    current: Option<ResourceVersion>,
    phase: ResourceRefreshPhase,
}

impl ResourceRefreshStatus {
    pub const fn current(self) -> Option<ResourceVersion> {
        self.current
    }

    pub const fn phase(self) -> ResourceRefreshPhase {
        self.phase
    }
}

/// 一次资源刷新 reservation。只有持有同一 reservation 的 caller 才能提交候选或记录失败。
#[derive(Clone, Eq, PartialEq)]
pub struct RefreshPermit {
    resource_id: ConfigId,
    epoch: u64,
    expected: Option<ResourceVersion>,
    token: u64,
}

impl RefreshPermit {
    pub fn resource_id(&self) -> &ConfigId {
        &self.resource_id
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub const fn expected_version(&self) -> Option<ResourceVersion> {
        self.expected
    }
}

impl fmt::Debug for RefreshPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RefreshPermit")
            .field("resource_id", &"[REDACTED]")
            .field("epoch", &self.epoch)
            .field("expected", &self.expected)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// 申请刷新时的拒绝原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshBeginError {
    AlreadyRefreshing {
        epoch: u64,
    },
    Backoff {
        retry_not_before: u64,
        consecutive_failures: u32,
    },
    EpochExhausted,
}

/// 候选发布时的拒绝原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefreshPublishError {
    UnknownAttempt,
    ResourceMismatch {
        expected: ConfigId,
        actual: ConfigId,
    },
    EpochMismatch {
        expected: u64,
        actual: u64,
    },
    Registry(ResourcePublishError),
}

/// 记录刷新失败后返回的退避结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefreshFailure {
    retry_not_before: u64,
    consecutive_failures: u32,
}

impl RefreshFailure {
    pub const fn retry_not_before(self) -> u64 {
        self.retry_not_before
    }

    pub const fn consecutive_failures(self) -> u32 {
        self.consecutive_failures
    }
}

#[derive(Clone, Copy)]
struct InFlight {
    epoch: u64,
    token: u64,
    expected: Option<ResourceVersion>,
}

#[derive(Default)]
struct ResourceState {
    next_epoch: u64,
    in_flight: Option<InFlight>,
    consecutive_failures: u32,
    retry_not_before: u64,
}

struct CoordinatorState<T> {
    registry: ResourceRegistrySnapshot<T>,
    resources: BTreeMap<ConfigId, ResourceState>,
    next_token: u64,
}

/// 纯内存资源刷新协调器。
///
/// registry 仍以 immutable snapshot 形式替换；协调器只负责把刷新生命周期串起来，
/// 不执行网络、磁盘或解析 I/O。`now` 是调用方提供的逻辑时间单位。
#[derive(Clone)]
pub struct ResourceRefreshCoordinator<T> {
    state: Arc<Mutex<CoordinatorState<T>>>,
    backoff: RefreshBackoff,
}

impl<T> ResourceRefreshCoordinator<T> {
    pub fn new(registry: ResourceRegistrySnapshot<T>, backoff: RefreshBackoff) -> Self {
        Self {
            state: Arc::new(Mutex::new(CoordinatorState {
                registry,
                resources: BTreeMap::new(),
                next_token: 0,
            })),
            backoff,
        }
    }

    pub fn current(&self) -> ResourceRegistrySnapshot<T>
    where
        T: Clone,
    {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .registry
            .clone()
    }

    pub fn status(&self, resource_id: &ConfigId) -> ResourceRefreshStatus {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = state
            .registry
            .lookup(resource_id)
            .map(|snapshot| snapshot.version());
        let phase =
            state
                .resources
                .get(resource_id)
                .map_or(ResourceRefreshPhase::Idle, |resource| {
                    match resource.in_flight {
                        Some(in_flight) => ResourceRefreshPhase::Refreshing {
                            epoch: in_flight.epoch,
                        },
                        None if resource.retry_not_before > 0 => ResourceRefreshPhase::Backoff {
                            retry_not_before: resource.retry_not_before,
                            consecutive_failures: resource.consecutive_failures,
                        },
                        None if resource.consecutive_failures > 0 => {
                            ResourceRefreshPhase::Backoff {
                                retry_not_before: resource.retry_not_before,
                                consecutive_failures: resource.consecutive_failures,
                            }
                        }
                        None => ResourceRefreshPhase::Idle,
                    }
                });
        ResourceRefreshStatus { current, phase }
    }

    /// 为资源创建唯一的刷新 reservation，并分配严格递增的 epoch。
    pub fn begin(
        &self,
        resource_id: &ConfigId,
        now: u64,
    ) -> Result<RefreshPermit, RefreshBeginError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = state
            .registry
            .lookup(resource_id)
            .map(|snapshot| snapshot.version());
        if let Some(in_flight) = state
            .resources
            .get(resource_id)
            .and_then(|resource| resource.in_flight)
        {
            return Err(RefreshBeginError::AlreadyRefreshing {
                epoch: in_flight.epoch,
            });
        }
        let retry_not_before = state
            .resources
            .get(resource_id)
            .map_or(0, |resource| resource.retry_not_before);
        let consecutive_failures = state
            .resources
            .get(resource_id)
            .map_or(0, |resource| resource.consecutive_failures);
        if now < retry_not_before {
            return Err(RefreshBeginError::Backoff {
                retry_not_before,
                consecutive_failures,
            });
        }

        let current_epoch = current.map_or(0, ResourceVersion::epoch);
        let next_epoch = state
            .resources
            .get(resource_id)
            .map_or(0, |resource| resource.next_epoch);
        let epoch = next_epoch
            .max(current_epoch)
            .checked_add(1)
            .ok_or(RefreshBeginError::EpochExhausted)?;
        let token = state.next_token;
        state.next_token = state.next_token.checked_add(1).unwrap_or(0);
        let resource = state.resources.entry(resource_id.clone()).or_default();
        resource.next_epoch = epoch;
        resource.in_flight = Some(InFlight {
            epoch,
            token,
            expected: current,
        });
        Ok(RefreshPermit {
            resource_id: resource_id.clone(),
            epoch,
            expected: current,
            token,
        })
    }

    /// 校验 reservation 和 epoch 后，使用 registry 的 CAS 语义发布候选。
    pub fn publish(
        &self,
        permit: &RefreshPermit,
        candidate: ResourceSnapshot<T>,
    ) -> Result<(), RefreshPublishError> {
        let actual_id = candidate.resource_id().clone();
        if actual_id != permit.resource_id {
            return Err(RefreshPublishError::ResourceMismatch {
                expected: permit.resource_id.clone(),
                actual: actual_id,
            });
        }
        if candidate.epoch() != permit.epoch {
            return Err(RefreshPublishError::EpochMismatch {
                expected: permit.epoch,
                actual: candidate.epoch(),
            });
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let in_flight = state
            .resources
            .get(&permit.resource_id)
            .and_then(|resource| resource.in_flight)
            .filter(|attempt| attempt.token == permit.token)
            .ok_or(RefreshPublishError::UnknownAttempt)?;
        let next_registry = state
            .registry
            .compare_and_publish(in_flight.expected, candidate)
            .map_err(RefreshPublishError::Registry)?;
        state.registry = next_registry;
        let resource = state
            .resources
            .get_mut(&permit.resource_id)
            .ok_or(RefreshPublishError::UnknownAttempt)?;
        resource.in_flight = None;
        resource.consecutive_failures = 0;
        resource.retry_not_before = 0;
        Ok(())
    }

    /// 结束 reservation 并安排下一次重试；失败不修改当前 immutable snapshot。
    pub fn fail(
        &self,
        permit: &RefreshPermit,
        now: u64,
    ) -> Result<RefreshFailure, RefreshPublishError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let resource = state
            .resources
            .get_mut(&permit.resource_id)
            .ok_or(RefreshPublishError::UnknownAttempt)?;
        resource
            .in_flight
            .filter(|attempt| attempt.token == permit.token)
            .ok_or(RefreshPublishError::UnknownAttempt)?;
        resource.in_flight = None;
        resource.consecutive_failures = resource.consecutive_failures.saturating_add(1);
        let shift = resource.consecutive_failures.saturating_sub(1).min(63);
        let delay = self
            .backoff
            .base_delay
            .checked_shl(shift)
            .unwrap_or(u64::MAX)
            .min(self.backoff.max_delay);
        resource.retry_not_before = now.saturating_add(delay);
        Ok(RefreshFailure {
            retry_not_before: resource.retry_not_before,
            consecutive_failures: resource.consecutive_failures,
        })
    }
}

impl<T> fmt::Debug for ResourceRefreshCoordinator<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceRefreshCoordinator")
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::resource::{ResourceSourceKind, ResourceStaleStatus};

    fn id(value: &str) -> ConfigId {
        ConfigId::new(value).expect("valid resource id")
    }

    fn snapshot(name: &str, epoch: u64, value: &str) -> ResourceSnapshot<String> {
        ResourceSnapshot::new(
            id(name),
            epoch,
            1,
            format!("hash-{value}"),
            format!("fingerprint-{value}"),
            "parser-v1",
            UNIX_EPOCH,
            ResourceSourceKind::File,
            false,
            ResourceStaleStatus::Fresh,
            value.to_owned(),
        )
    }

    fn coordinator() -> ResourceRefreshCoordinator<String> {
        let registry = ResourceRegistrySnapshot::new()
            .publish(snapshot("hosts", 1, "old"))
            .unwrap();
        ResourceRefreshCoordinator::new(registry, RefreshBackoff::new(2, 8).unwrap())
    }

    #[test]
    fn only_one_concurrent_refresh_is_allowed_per_resource() {
        let coordinator = Arc::new(coordinator());
        let handles = (0..16)
            .map(|_| {
                let coordinator = Arc::clone(&coordinator);
                thread::spawn(move || coordinator.begin(&id("hosts"), 0))
            })
            .collect::<Vec<_>>();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(RefreshBeginError::AlreadyRefreshing { .. })))
                .count(),
            15
        );
    }

    #[test]
    fn rejects_out_of_order_candidate_without_changing_reservation() {
        let coordinator = coordinator();
        let permit = coordinator.begin(&id("hosts"), 0).unwrap();
        let rejected = coordinator.publish(&permit, snapshot("hosts", 1, "old-epoch"));
        assert_eq!(
            rejected,
            Err(RefreshPublishError::EpochMismatch {
                expected: 2,
                actual: 1,
            })
        );
        assert_eq!(
            coordinator.status(&id("hosts")).phase(),
            ResourceRefreshPhase::Refreshing { epoch: 2 }
        );
        coordinator
            .publish(&permit, snapshot("hosts", 2, "new"))
            .unwrap();
        assert_eq!(
            coordinator
                .current()
                .lookup(&id("hosts"))
                .unwrap()
                .compiled(),
            "new"
        );
    }

    #[test]
    fn failure_uses_deterministic_capped_backoff_and_recovers() {
        let coordinator = coordinator();
        let first = coordinator.begin(&id("hosts"), 10).unwrap();
        assert_eq!(
            coordinator.fail(&first, 10).unwrap(),
            RefreshFailure {
                retry_not_before: 12,
                consecutive_failures: 1,
            }
        );
        assert!(matches!(
            coordinator.begin(&id("hosts"), 11),
            Err(RefreshBeginError::Backoff {
                retry_not_before: 12,
                consecutive_failures: 1
            })
        ));
        let second = coordinator.begin(&id("hosts"), 12).unwrap();
        assert_eq!(
            coordinator.fail(&second, 20).unwrap().retry_not_before(),
            24
        );
        let third = coordinator.begin(&id("hosts"), 24).unwrap();
        coordinator
            .publish(&third, snapshot("hosts", 4, "recovered"))
            .unwrap();
        assert_eq!(
            coordinator.status(&id("hosts")).phase(),
            ResourceRefreshPhase::Idle
        );
    }

    #[test]
    fn permits_are_resource_and_attempt_scoped() {
        let coordinator = coordinator();
        let permit = coordinator.begin(&id("hosts"), 0).unwrap();
        let wrong_resource = snapshot("other", permit.epoch(), "wrong");
        assert!(matches!(
            coordinator.publish(&permit, wrong_resource),
            Err(RefreshPublishError::ResourceMismatch { .. })
        ));
        coordinator.fail(&permit, 0).unwrap();
        assert_eq!(
            coordinator.fail(&permit, 2),
            Err(RefreshPublishError::UnknownAttempt)
        );
    }
}
