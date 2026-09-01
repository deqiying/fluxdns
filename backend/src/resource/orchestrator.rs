//! Resource refresh scheduler 与 coordinator 的 Runtime-facing 纯逻辑边界。
//!
//! 该模块只编排 due、single-flight、backoff、CAS publish 和 stop 状态；实际
//! fetch、parse、persist 与 Runtime task 生命周期由后续 adapter/worker 提供。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::config::resolve::ConfigId;

use super::{
    RefreshBeginError, RefreshFailure, RefreshPermit, RefreshPublishError,
    ResourceRefreshCoordinator, ResourceRefreshPhase, ResourceRefreshStatus,
    ResourceRegistrySnapshot, ResourceSchedule, ResourceScheduleDecision, ResourceSchedulePolicy,
    ResourceSnapshot,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceRefreshRuntimeBeginError {
    UnknownResource,
    NotDue(ResourceScheduleDecision),
    Refresh(RefreshBeginError),
}

/// 单资源 schedule 与 immutable registry coordinator 的组合 facade。
#[derive(Clone)]
pub struct ResourceRefreshRuntime<T> {
    coordinator: ResourceRefreshCoordinator<T>,
    schedules: Arc<Mutex<BTreeMap<ConfigId, ResourceSchedule>>>,
    policy: ResourceSchedulePolicy,
}

/// 绑定 schedule 的一次刷新 reservation。
pub struct ResourceRefreshRuntimePermit {
    resource_id: ConfigId,
    permit: RefreshPermit,
}

impl ResourceRefreshRuntimePermit {
    pub fn resource_id(&self) -> &ConfigId {
        &self.resource_id
    }

    pub const fn epoch(&self) -> u64 {
        self.permit.epoch()
    }
}

impl<T> ResourceRefreshRuntime<T> {
    pub fn new(
        registry: ResourceRegistrySnapshot<T>,
        policy: ResourceSchedulePolicy,
        initial_due: u64,
    ) -> Self {
        let schedules = registry
            .summary()
            .into_iter()
            .map(|(resource_id, _)| (resource_id, ResourceSchedule::new(policy, initial_due)))
            .collect();
        Self {
            coordinator: ResourceRefreshCoordinator::new(registry, policy.backoff()),
            schedules: Arc::new(Mutex::new(schedules)),
            policy,
        }
    }

    pub fn current(&self) -> ResourceRegistrySnapshot<T>
    where
        T: Clone,
    {
        self.coordinator.current()
    }

    pub fn register(&self, resource_id: ConfigId, initial_due: u64) -> bool {
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if schedules.contains_key(&resource_id) {
            return false;
        }
        schedules.insert(resource_id, ResourceSchedule::new(self.policy, initial_due));
        true
    }

    pub fn decision(&self, resource_id: &ConfigId, now: u64) -> Option<ResourceScheduleDecision> {
        self.schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(resource_id)
            .map(|schedule| schedule.decision(now))
    }

    pub fn status(&self, resource_id: &ConfigId) -> ResourceRefreshStatus {
        self.coordinator.status(resource_id)
    }

    pub fn begin_due(
        &self,
        resource_id: &ConfigId,
        now: u64,
    ) -> Result<ResourceRefreshRuntimePermit, ResourceRefreshRuntimeBeginError> {
        let decision = self
            .decision(resource_id, now)
            .ok_or(ResourceRefreshRuntimeBeginError::UnknownResource)?;
        if !decision.is_due() {
            return Err(ResourceRefreshRuntimeBeginError::NotDue(decision));
        }
        let permit = self
            .coordinator
            .begin(resource_id, now)
            .map_err(ResourceRefreshRuntimeBeginError::Refresh)?;
        Ok(ResourceRefreshRuntimePermit {
            resource_id: resource_id.clone(),
            permit,
        })
    }

    pub fn publish(
        &self,
        permit: ResourceRefreshRuntimePermit,
        candidate: ResourceSnapshot<T>,
        completed_at: u64,
    ) -> Result<(), RefreshPublishError> {
        self.coordinator.publish(&permit.permit, candidate)?;
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(schedule) = schedules.get_mut(&permit.resource_id) {
            let _ = schedule.record_success(completed_at);
        }
        Ok(())
    }

    pub fn fail(
        &self,
        permit: ResourceRefreshRuntimePermit,
        failed_at: u64,
    ) -> Result<RefreshFailure, RefreshPublishError> {
        let failure = self.coordinator.fail(&permit.permit, failed_at)?;
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(schedule) = schedules.get_mut(&permit.resource_id) {
            let _ = schedule.record_failure(failed_at);
        }
        Ok(failure)
    }

    pub fn cancel(&self, resource_id: &ConfigId, permit: ResourceRefreshRuntimePermit) -> bool {
        let cancelled = self.coordinator.cancel(resource_id, &permit.permit);
        if cancelled
            && let Some(schedule) = self
                .schedules
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_mut(resource_id)
        {
            let _ = schedule.cancel();
        }
        cancelled
    }

    pub fn shutdown(&self) {
        let mut schedules = self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for schedule in schedules.values_mut() {
            let _ = schedule.shutdown();
        }
        drop(schedules);
        for resource_id in self
            .schedules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            let _ = self.coordinator.cancel_resource(&resource_id);
        }
    }

    pub fn phase(&self, resource_id: &ConfigId) -> ResourceRefreshPhase {
        self.status(resource_id).phase()
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::resource::{
        RefreshBackoff, ResourceScheduleStopReason, ResourceSourceKind, ResourceStaleStatus,
    };

    fn id(value: &str) -> ConfigId {
        ConfigId::new(value).expect("valid resource id")
    }

    fn policy() -> ResourceSchedulePolicy {
        ResourceSchedulePolicy::new(10, 3, 3, RefreshBackoff::new(2, 8).unwrap())
            .expect("valid schedule policy")
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

    fn runtime() -> ResourceRefreshRuntime<String> {
        let registry = ResourceRegistrySnapshot::new()
            .publish(snapshot("hosts", 1, "old"))
            .unwrap();
        ResourceRefreshRuntime::new(registry, policy(), 0)
    }

    #[test]
    fn due_reservation_publishes_and_reschedules() {
        let runtime = runtime();
        let permit = runtime.begin_due(&id("hosts"), 0).unwrap();
        assert_eq!(permit.resource_id(), &id("hosts"));
        assert_eq!(permit.epoch(), 2);
        assert_eq!(
            runtime.status(&id("hosts")).phase(),
            ResourceRefreshPhase::Refreshing { epoch: 2 }
        );

        runtime
            .publish(permit, snapshot("hosts", 2, "new"), 5)
            .unwrap();
        assert_eq!(
            runtime.current().lookup(&id("hosts")).unwrap().compiled(),
            "new"
        );
        assert_eq!(
            runtime.decision(&id("hosts"), 14).unwrap().next_due(),
            Some(15)
        );
        assert_eq!(
            runtime.status(&id("hosts")).phase(),
            ResourceRefreshPhase::Idle
        );
    }

    #[test]
    fn failure_backoff_blocks_until_retry_deadline() {
        let runtime = runtime();
        let permit = runtime.begin_due(&id("hosts"), 0).unwrap();
        let failure = runtime.fail(permit, 0).unwrap();
        assert_eq!(failure.retry_not_before(), 2);
        assert!(matches!(
            runtime.begin_due(&id("hosts"), 1),
            Err(ResourceRefreshRuntimeBeginError::NotDue(
                ResourceScheduleDecision::Waiting {
                    next_due: 2,
                    consecutive_failures: 1,
                    stale: false,
                }
            ))
        ));
        assert!(runtime.begin_due(&id("hosts"), 2).is_ok());
    }

    #[test]
    fn cancel_releases_reservation_and_stops_future_work() {
        let runtime = runtime();
        let permit = runtime.begin_due(&id("hosts"), 0).unwrap();
        assert!(runtime.cancel(&id("hosts"), permit));
        assert_eq!(
            runtime.decision(&id("hosts"), 100),
            Some(ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Cancelled,
            })
        );
        assert!(matches!(
            runtime.begin_due(&id("hosts"), 100),
            Err(ResourceRefreshRuntimeBeginError::NotDue(
                ResourceScheduleDecision::Stopped {
                    reason: ResourceScheduleStopReason::Cancelled,
                }
            ))
        ));
    }

    #[test]
    fn shutdown_stops_all_registered_resources_and_clears_inflight() {
        let runtime = runtime();
        assert!(runtime.register(id("rules"), 0));
        let permit = runtime.begin_due(&id("hosts"), 0).unwrap();
        runtime.shutdown();
        assert!(matches!(
            runtime.decision(&id("hosts"), 0),
            Some(ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Shutdown,
            })
        ));
        assert!(matches!(
            runtime.decision(&id("rules"), 0),
            Some(ResourceScheduleDecision::Stopped {
                reason: ResourceScheduleStopReason::Shutdown,
            })
        ));
        assert!(!runtime.cancel(&id("hosts"), permit));
    }
}
