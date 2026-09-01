use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use thiserror::Error;

use crate::config::resolve::ConfigId;
use crate::dns::RuntimeRevision;
use crate::ports::effects::ResourceFetcher;
use crate::resource::{ResourceScheduleDecision, ResourceSnapshot, RuleIndex};

use super::bind::{BoundCandidate, BoundListenerSet};
use super::prepared::{PreparedRuntime, ResourceRefreshError};
use super::snapshot::RuntimeSnapshot;

/// 已发布、可接收请求的运行时实例。
pub struct ActiveRuntime {
    prepared: PreparedRuntime,
    listeners: BoundListenerSet,
    admission: Arc<AdmissionState>,
}

impl ActiveRuntime {
    fn from_candidate(candidate: BoundCandidate) -> Self {
        let (prepared, listeners) = candidate.into_parts();
        Self {
            prepared,
            listeners,
            admission: Arc::new(AdmissionState::default()),
        }
    }

    pub fn snapshot(&self) -> &RuntimeSnapshot {
        self.prepared.snapshot()
    }

    pub fn revision(&self) -> RuntimeRevision {
        self.snapshot().revision()
    }

    pub fn listeners(&self) -> &BoundListenerSet {
        &self.listeners
    }

    pub fn resource_fetcher(&self) -> Option<Arc<dyn ResourceFetcher>> {
        self.prepared.resource_fetcher()
    }

    pub fn resource_worker_ids(&self) -> Vec<ConfigId> {
        self.prepared.resource_worker_ids()
    }

    pub fn resource_refresh_decision(
        &self,
        resource: &ConfigId,
        now: u64,
    ) -> Option<ResourceScheduleDecision> {
        self.prepared.resource_refresh_decision(resource, now)
    }

    pub async fn refresh_remote_rule_set(
        &self,
        resource: &ConfigId,
        now: u64,
        deadline: crate::dns::Deadline,
        cancellation: crate::dns::Cancellation,
    ) -> Result<ResourceSnapshot<RuleIndex>, ResourceRefreshError> {
        self.prepared
            .refresh_remote_rule_set(resource, now, deadline, cancellation)
            .await
    }

    pub fn shutdown_resource_refresh(&self) {
        self.prepared.shutdown_resource_refresh();
    }

    /// 尝试为一个请求建立 guard；drain 开始后不再接收新请求。
    pub fn try_acquire(&self) -> Result<RequestGuard, AdmissionError> {
        let mut active = self.admission.active.load(Ordering::Acquire);
        loop {
            if self.admission.draining.load(Ordering::Acquire) {
                return Err(AdmissionError::Draining);
            }
            if active == usize::MAX {
                return Err(AdmissionError::Capacity);
            }
            match self.admission.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Drain may begin between the first check and the increment. The
                    // second check establishes the admission linearization point.
                    if self.admission.draining.load(Ordering::Acquire) {
                        self.admission.active.fetch_sub(1, Ordering::AcqRel);
                        return Err(AdmissionError::Draining);
                    }
                    return Ok(RequestGuard {
                        admission: Arc::clone(&self.admission),
                    });
                }
                Err(observed) => active = observed,
            }
        }
    }

    pub fn active_requests(&self) -> usize {
        self.admission.active.load(Ordering::Acquire)
    }

    /// 标记实例进入 drain；返回值表示本次调用是否完成了状态切换。
    pub fn begin_drain(&self) -> bool {
        self.admission
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn is_draining(&self) -> bool {
        self.admission.draining.load(Ordering::Acquire)
    }

    fn into_candidate(self) -> BoundCandidate {
        BoundCandidate::from_parts(self.prepared, self.listeners)
    }
}

impl fmt::Debug for ActiveRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveRuntime")
            .field("revision", &self.revision())
            .field("listener_count", &self.listeners.len())
            .field("active_requests", &self.active_requests())
            .field("draining", &self.is_draining())
            .finish()
    }
}

#[derive(Default)]
struct AdmissionState {
    draining: AtomicBool,
    active: AtomicUsize,
}

/// 绑定到单个 ActiveRuntime 的请求生命周期 guard。
pub struct RequestGuard {
    admission: Arc<AdmissionState>,
}

impl fmt::Debug for RequestGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestGuard")
            .field(
                "active_requests",
                &self.admission.active.load(Ordering::Acquire),
            )
            .finish()
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let previous = self.admission.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "request guard count must not underflow");
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AdmissionError {
    #[error("runtime is draining and does not accept new requests")]
    Draining,
    #[error("runtime request admission capacity is exhausted")]
    Capacity,
}

/// 捕获同一个 ActiveRuntime 和对应请求 guard 的 lease。
pub struct RuntimeLease {
    runtime: Arc<ActiveRuntime>,
    _guard: RequestGuard,
}

impl RuntimeLease {
    pub fn runtime(&self) -> &ActiveRuntime {
        &self.runtime
    }

    pub fn snapshot(&self) -> &RuntimeSnapshot {
        self.runtime.snapshot()
    }

    pub fn revision(&self) -> RuntimeRevision {
        self.runtime.revision()
    }
}

impl Deref for RuntimeLease {
    type Target = ActiveRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl fmt::Debug for RuntimeLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeLease")
            .field("revision", &self.revision())
            .field("active_requests", &self.active_requests())
            .finish()
    }
}

/// 以 ArcSwap 原子持有唯一对外可见的 ActiveRuntime。
pub struct RuntimeCoordinator {
    active: ArcSwap<ActiveRuntime>,
}

impl RuntimeCoordinator {
    pub fn new(initial: BoundCandidate) -> Self {
        Self {
            active: ArcSwap::from(Arc::new(ActiveRuntime::from_candidate(initial))),
        }
    }

    pub fn load(&self) -> Arc<ActiveRuntime> {
        self.active.load_full()
    }

    pub fn current_revision(&self) -> RuntimeRevision {
        self.load().revision()
    }

    pub fn acquire(&self) -> Result<RuntimeLease, AdmissionError> {
        let runtime = self.load();
        let guard = runtime.try_acquire()?;
        Ok(RuntimeLease {
            runtime,
            _guard: guard,
        })
    }

    /// 无条件发布候选，并把旧实例标记为 draining。
    pub fn activate(&self, candidate: BoundCandidate) -> Arc<ActiveRuntime> {
        let next = Arc::new(ActiveRuntime::from_candidate(candidate));
        let previous = self.active.swap(next);
        previous.begin_drain();
        previous
    }

    /// 只有当前 revision 仍与预期一致时才发布候选；失败时返还候选供调用方重试。
    pub fn compare_and_activate(
        &self,
        expected: RuntimeRevision,
        candidate: BoundCandidate,
    ) -> Result<Arc<ActiveRuntime>, ActivationError> {
        let current = self.load();
        if current.revision() != expected {
            return Err(ActivationError {
                expected,
                actual: current.revision(),
                candidate,
            });
        }

        let next = Arc::new(ActiveRuntime::from_candidate(candidate));
        let observed = self.active.compare_and_swap(&current, Arc::clone(&next));
        if Arc::ptr_eq(&*observed, &current) {
            current.begin_drain();
            return Ok(current);
        }

        let actual = observed.revision();
        let candidate = Arc::try_unwrap(next)
            .map(ActiveRuntime::into_candidate)
            .unwrap_or_else(|_| unreachable!("CAS candidate has no other owners"));
        Err(ActivationError {
            expected,
            actual,
            candidate,
        })
    }
}

#[derive(Debug, Error)]
#[error("runtime activation CAS lost: expected revision {expected:?}, current revision {actual:?}")]
pub struct ActivationError {
    expected: RuntimeRevision,
    actual: RuntimeRevision,
    candidate: BoundCandidate,
}

impl ActivationError {
    pub fn expected(&self) -> RuntimeRevision {
        self.expected
    }

    pub fn actual(&self) -> RuntimeRevision {
        self.actual
    }

    pub fn into_candidate(self) -> BoundCandidate {
        self.candidate
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::RuntimeRevision;

    use super::{AdmissionError, RuntimeCoordinator};
    use crate::runtime::PreparedRuntime;

    fn candidate(revision: u64) -> crate::runtime::BoundCandidate {
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(include_str!("../../../config-example.yaml"))
            .expect("repository example must remain a valid runtime fixture")
            .resolved;
        let prepared = PreparedRuntime::prepare(config, RuntimeRevision(revision)).unwrap();
        super::super::bind::test_candidate(prepared)
    }

    #[test]
    fn coordinator_load_and_acquire_capture_one_active_revision() {
        let coordinator = RuntimeCoordinator::new(candidate(1));
        let lease = coordinator.acquire().unwrap();

        assert_eq!(lease.revision(), RuntimeRevision(1));
        assert_eq!(lease.active_requests(), 1);
        assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
        drop(lease);
        assert_eq!(coordinator.load().active_requests(), 0);
    }

    #[test]
    fn draining_runtime_rejects_new_requests_but_releases_existing_guard() {
        let coordinator = RuntimeCoordinator::new(candidate(1));
        let runtime = coordinator.load();
        let guard = runtime.try_acquire().unwrap();

        assert!(runtime.begin_drain());
        assert!(!runtime.begin_drain());
        assert_eq!(runtime.active_requests(), 1);
        assert!(matches!(
            runtime.try_acquire(),
            Err(AdmissionError::Draining)
        ));
        drop(guard);
        assert_eq!(runtime.active_requests(), 0);
    }

    #[test]
    fn activation_swaps_revision_and_drains_previous_runtime() {
        let coordinator = RuntimeCoordinator::new(candidate(1));
        let previous = coordinator.activate(candidate(2));

        assert_eq!(previous.revision(), RuntimeRevision(1));
        assert!(previous.is_draining());
        assert_eq!(coordinator.current_revision(), RuntimeRevision(2));
        assert_eq!(
            coordinator.acquire().unwrap().revision(),
            RuntimeRevision(2)
        );
    }

    #[test]
    fn failed_cas_returns_candidate_for_a_deterministic_retry() {
        let coordinator = RuntimeCoordinator::new(candidate(1));
        let error = coordinator
            .compare_and_activate(RuntimeRevision(99), candidate(2))
            .unwrap_err();

        assert_eq!(error.expected(), RuntimeRevision(99));
        assert_eq!(error.actual(), RuntimeRevision(1));
        let candidate = error.into_candidate();
        let previous = coordinator
            .compare_and_activate(RuntimeRevision(1), candidate)
            .unwrap();

        assert_eq!(previous.revision(), RuntimeRevision(1));
        assert!(previous.is_draining());
        assert_eq!(coordinator.current_revision(), RuntimeRevision(2));
    }

    #[test]
    fn lease_keeps_old_runtime_alive_after_atomic_swap() {
        let coordinator = Arc::new(RuntimeCoordinator::new(candidate(1)));
        let lease = coordinator.acquire().unwrap();
        let previous = coordinator.activate(candidate(2));

        assert_eq!(lease.revision(), RuntimeRevision(1));
        assert!(previous.is_draining());
        assert_eq!(lease.runtime().active_requests(), 1);
        drop(lease);
        assert_eq!(previous.active_requests(), 0);
    }
}
