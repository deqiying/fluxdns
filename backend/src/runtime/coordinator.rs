use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use thiserror::Error;

use crate::cache::LateCacheFinalizer;
use crate::config::resolve::ConfigId;
use crate::dns::{PolicyDnsCore, RuntimeRevision};
use crate::ports::effects::{ResourceFetcher, SocketFactory};
use crate::resource::{ResourceScheduleDecision, ResourceSnapshot, RuleIndex};

use super::bind::{BindError, BoundCandidate, BoundListenerSet, bind_prepared};
use super::prepared::{PreparedRuntime, RefreshedResourceSnapshot, ResourceRefreshError};
use super::snapshot::RuntimeSnapshot;

/// 已发布、可接收请求的运行时实例。
pub struct ActiveRuntime {
    prepared: PreparedRuntime,
    listeners: Arc<BoundListenerSet>,
    admission: Arc<AdmissionState>,
}

impl ActiveRuntime {
    fn from_candidate(candidate: BoundCandidate) -> Self {
        let (prepared, listeners) = candidate.into_parts();
        Self {
            prepared,
            listeners: Arc::new(listeners),
            admission: Arc::new(AdmissionState::default()),
        }
    }

    fn from_prepared_and_listeners(
        prepared: PreparedRuntime,
        listeners: Arc<BoundListenerSet>,
    ) -> Self {
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

    pub fn bind_plan(&self) -> &crate::config::BindPlan {
        self.prepared.bind_plan()
    }

    pub fn resource_fetcher(&self) -> Option<Arc<dyn ResourceFetcher>> {
        self.prepared.resource_fetcher()
    }

    fn finalizer_owner(&self) -> Option<Arc<LateCacheFinalizer>> {
        self.snapshot()
            .policy_core()
            .map(PolicyDnsCore::finalizer_owner)
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

    pub async fn refresh_resource(
        &self,
        resource: &ConfigId,
        now: u64,
        deadline: crate::dns::Deadline,
        cancellation: crate::dns::Cancellation,
    ) -> Result<RefreshedResourceSnapshot, ResourceRefreshError> {
        self.prepared
            .refresh_resource(resource, now, deadline, cancellation)
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
        let listeners = Arc::try_unwrap(self.listeners)
            .unwrap_or_else(|_| unreachable!("candidate listener set must be uniquely owned"));
        BoundCandidate::from_parts(self.prepared, listeners)
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
    mutation: tokio::sync::Mutex<()>,
    finalizer_owners: std::sync::Mutex<Vec<Arc<LateCacheFinalizer>>>,
}

impl RuntimeCoordinator {
    pub fn new(initial: BoundCandidate) -> Self {
        let active = Arc::new(ActiveRuntime::from_candidate(initial));
        Self {
            active: ArcSwap::from(Arc::clone(&active)),
            mutation: tokio::sync::Mutex::new(()),
            finalizer_owners: std::sync::Mutex::new(active.finalizer_owner().into_iter().collect()),
        }
    }

    pub(crate) fn from_active(initial: Arc<ActiveRuntime>) -> Self {
        Self {
            active: ArcSwap::from(Arc::clone(&initial)),
            mutation: tokio::sync::Mutex::new(()),
            finalizer_owners: std::sync::Mutex::new(
                initial.finalizer_owner().into_iter().collect(),
            ),
        }
    }

    fn register_finalizer_owner(&self, runtime: &ActiveRuntime) {
        let Some(owner) = runtime.finalizer_owner() else {
            return;
        };
        let mut owners = self
            .finalizer_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owners.iter().any(|current| Arc::ptr_eq(current, &owner)) {
            return;
        }
        owners.push(owner);
    }

    /// 在统一 deadline 内关闭所有曾由该 coordinator 发布的 cache finalizer。
    pub(crate) async fn shutdown_finalizers(&self, deadline: crate::dns::Deadline) -> bool {
        let owners = self
            .finalizer_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut completed = true;
        for owner in owners {
            if !owner.shutdown_until(deadline).await {
                completed = false;
            }
        }
        completed
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

    pub fn resource_worker_ids(&self) -> Vec<ConfigId> {
        self.load().resource_worker_ids()
    }

    pub fn resource_refresh_decision(
        &self,
        resource: &ConfigId,
        now: u64,
    ) -> Option<ResourceScheduleDecision> {
        self.load().resource_refresh_decision(resource, now)
    }

    pub async fn refresh_resource(
        &self,
        resource: &ConfigId,
        now: u64,
        deadline: crate::dns::Deadline,
        cancellation: crate::dns::Cancellation,
    ) -> Result<RefreshedResourceSnapshot, ResourceRefreshError> {
        let _mutation = self.mutation.lock().await;
        self.load()
            .refresh_resource(resource, now, deadline, cancellation)
            .await
    }

    pub async fn refresh_resource_if_current(
        &self,
        expected: &Arc<ActiveRuntime>,
        resource: &ConfigId,
        now: u64,
        deadline: crate::dns::Deadline,
        cancellation: crate::dns::Cancellation,
    ) -> Result<RefreshedResourceSnapshot, ResourceRefreshCoordinatorError> {
        let _mutation = self.mutation.lock().await;
        let current = self.load();
        if !Arc::ptr_eq(&current, expected) {
            return Err(ResourceRefreshCoordinatorError::Stale {
                expected: expected.revision(),
                actual: current.revision(),
            });
        }
        let refreshed = expected
            .refresh_resource(resource, now, deadline, cancellation)
            .await
            .map_err(ResourceRefreshCoordinatorError::Resource)?;
        let current = self.load();
        if !Arc::ptr_eq(&current, expected) {
            return Err(ResourceRefreshCoordinatorError::Stale {
                expected: expected.revision(),
                actual: current.revision(),
            });
        }
        Ok(refreshed)
    }

    pub fn shutdown_resource_refresh(&self) {
        self.load().shutdown_resource_refresh();
    }

    pub async fn bind_and_activate(
        &self,
        expected: RuntimeRevision,
        prepared: PreparedRuntime,
        factory: &dyn SocketFactory,
        deadline: crate::dns::Deadline,
        cancellation: &crate::dns::Cancellation,
    ) -> Result<Arc<ActiveRuntime>, RuntimeReloadError> {
        let candidate = bind_prepared(prepared, factory, deadline, cancellation)
            .await
            .map_err(RuntimeReloadError::Bind)?;
        self.compare_and_activate_serialized(expected, candidate)
            .await
            .map_err(RuntimeReloadError::Activation)?;
        Ok(self.load())
    }

    /// 在同一 mutation gate 下合并候选状态并执行 revision CAS。
    pub async fn compare_and_activate_serialized(
        &self,
        expected: RuntimeRevision,
        candidate: BoundCandidate,
    ) -> Result<Arc<ActiveRuntime>, ActivationError> {
        let _mutation = self.mutation.lock().await;
        self.compare_and_activate(expected, candidate)
    }

    /// 在 BindPlan 未变化时复用当前已激活 listener，只切换 prepared Runtime。
    pub async fn activate_prepared_reusing_listeners(
        &self,
        expected: RuntimeRevision,
        prepared: PreparedRuntime,
    ) -> Result<Arc<ActiveRuntime>, RuntimeReuseError> {
        let _mutation = self.mutation.lock().await;
        let current = self.load();
        if current.revision() != expected {
            return Err(RuntimeReuseError::RevisionMismatch {
                expected,
                actual: current.revision(),
            });
        }
        if current.bind_plan() != prepared.bind_plan() {
            return Err(RuntimeReuseError::BindPlanChanged);
        }
        let next = Arc::new(ActiveRuntime::from_prepared_and_listeners(
            prepared,
            Arc::clone(&current.listeners),
        ));
        let observed = self.active.compare_and_swap(&current, Arc::clone(&next));
        if Arc::ptr_eq(&*observed, &current) {
            self.register_finalizer_owner(&next);
            current.begin_drain();
            return Ok(next);
        }
        Err(RuntimeReuseError::RevisionMismatch {
            expected,
            actual: observed.revision(),
        })
    }

    /// 无条件发布候选，并把旧实例标记为 draining。
    pub fn activate(&self, candidate: BoundCandidate) -> Arc<ActiveRuntime> {
        let next = Arc::new(ActiveRuntime::from_candidate(candidate));
        self.register_finalizer_owner(&next);
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
                candidate: Box::new(candidate),
            });
        }

        let (mut prepared, listeners) = candidate.into_parts();
        prepared.merge_state_from(&current.prepared);
        let candidate = BoundCandidate::from_parts(prepared, listeners);
        let next = Arc::new(ActiveRuntime::from_candidate(candidate));
        let observed = self.active.compare_and_swap(&current, Arc::clone(&next));
        if Arc::ptr_eq(&*observed, &current) {
            self.register_finalizer_owner(&next);
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
            candidate: Box::new(candidate),
        })
    }
}

#[derive(Debug, Error)]
#[error("runtime activation CAS lost: expected revision {expected:?}, current revision {actual:?}")]
pub struct ActivationError {
    expected: RuntimeRevision,
    actual: RuntimeRevision,
    candidate: Box<BoundCandidate>,
}

impl ActivationError {
    pub fn expected(&self) -> RuntimeRevision {
        self.expected
    }

    pub fn actual(&self) -> RuntimeRevision {
        self.actual
    }

    pub fn into_candidate(self) -> BoundCandidate {
        *self.candidate
    }
}

#[derive(Debug, Error)]
pub enum RuntimeReloadError {
    #[error("runtime candidate bind failed: {0}")]
    Bind(#[source] BindError),
    #[error("runtime candidate activation failed: {0}")]
    Activation(#[source] ActivationError),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RuntimeReuseError {
    #[error("runtime reuse requires the same bind plan")]
    BindPlanChanged,
    #[error(
        "runtime reuse observed a different active revision: expected {expected:?}, current {actual:?}"
    )]
    RevisionMismatch {
        expected: RuntimeRevision,
        actual: RuntimeRevision,
    },
}

#[derive(Debug, Error)]
pub enum ResourceRefreshCoordinatorError {
    #[error(
        "resource refresh observed a different active runtime: expected {expected:?}, current {actual:?}"
    )]
    Stale {
        expected: RuntimeRevision,
        actual: RuntimeRevision,
    },
    #[error(transparent)]
    Resource(#[from] ResourceRefreshError),
}

impl RuntimeReloadError {
    pub fn into_candidate(self) -> Option<BoundCandidate> {
        match self {
            Self::Bind(_) => None,
            Self::Activation(error) => Some(error.into_candidate()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::config::resolve::ConfigId;
    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::{Cancellation, Deadline, RuntimeRevision};
    use crate::ports::effects::{
        ActivatedSocket, ActivatedSocketHandle, PreparedSocket, SocketFactory, SocketKind,
        SocketSpec,
    };
    use crate::ports::{PortError, PortErrorClass, PortFuture};

    use super::{
        AdmissionError, ResourceRefreshCoordinatorError, RuntimeCoordinator, RuntimeReloadError,
    };
    use crate::runtime::PreparedRuntime;

    fn candidate(revision: u64) -> crate::runtime::BoundCandidate {
        let (source, _) = crate::config::test_support::portable_example();
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("repository example must remain a valid runtime fixture")
            .resolved;
        let prepared = PreparedRuntime::prepare(config, RuntimeRevision(revision)).unwrap();
        super::super::bind::test_candidate(prepared)
    }

    #[derive(Clone, Copy)]
    struct TestSocketFactory;

    struct TestPreparedSocket {
        spec: SocketSpec,
    }

    struct TestActivatedSocket {
        spec: SocketSpec,
    }

    impl SocketFactory for TestSocketFactory {
        fn prepare<'a>(
            &'a self,
            spec: SocketSpec,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Box<dyn PreparedSocket>, PortError>> {
            Box::pin(
                async move { Ok(Box::new(TestPreparedSocket { spec }) as Box<dyn PreparedSocket>) },
            )
        }
    }

    impl PreparedSocket for TestPreparedSocket {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(self.spec.address)
        }

        fn activate(self: Box<Self>) -> Result<Box<dyn ActivatedSocket>, PortError> {
            Ok(Box::new(TestActivatedSocket { spec: self.spec }))
        }
    }

    impl ActivatedSocket for TestActivatedSocket {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(self.spec.address)
        }

        fn kind(&self) -> SocketKind {
            self.spec.kind
        }

        fn socket_handle(&self) -> Result<ActivatedSocketHandle, PortError> {
            Err(PortError::new(
                PortErrorClass::Unavailable,
                "test_socket.handle",
            ))
        }
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

    #[tokio::test]
    async fn coordinator_refreshes_resource_on_current_active_runtime() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-coordinator-resource-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("hosts.txt");
        std::fs::write(&path, "192.0.2.10 old.example\n").unwrap();
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&format!(
                r#"
version: 1
work:
  path: {root}
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {{}}
listener:
  - type: udp
    name: dns
    addresses: [127.0.0.1]
    port: 5300
    strategy: default
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "127.0.0.1 fallback.test"
hosts:
  - type: file
    name: local-hosts
    format: hosts
    path: {path}
    auto_update: true
    update_interval: 1s
rule_set: []
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
clients: []
outbound: []
"#,
                root = root.display(),
                path = path.display(),
            ))
            .unwrap()
            .resolved;
        let prepared = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
            Arc::clone(&config),
            RuntimeRevision(7),
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = RuntimeCoordinator::new(super::super::bind::test_candidate(prepared));

        std::fs::write(&path, "192.0.2.11 new.example\n").unwrap();
        let resource = ConfigId::new("local-hosts").unwrap();
        let refreshed = coordinator
            .refresh_resource(
                &resource,
                u64::MAX,
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                Cancellation::new(),
            )
            .await
            .unwrap();

        assert_eq!(refreshed.epoch(), 2);
        assert_eq!(
            coordinator
                .load()
                .snapshot()
                .resources()
                .lookup(&resource)
                .unwrap()
                .version(),
            crate::resource::ResourceVersion::new(2, 0)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn coordinator_rejects_refresh_for_a_stale_active_runtime() {
        let coordinator = RuntimeCoordinator::new(candidate(1));
        let expected = coordinator.load();
        coordinator.activate(candidate(2));

        let error = coordinator
            .refresh_resource_if_current(
                &expected,
                &ConfigId::new("not-configured").unwrap(),
                u64::MAX,
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                Cancellation::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ResourceRefreshCoordinatorError::Stale {
                expected: RuntimeRevision(1),
                actual: RuntimeRevision(2),
            }
        ));
    }

    #[tokio::test]
    async fn bind_and_activate_publishes_a_prepared_candidate_after_binding() {
        let coordinator = RuntimeCoordinator::new(candidate(1));
        let (source, _) = crate::config::test_support::portable_example();
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .unwrap()
            .resolved;
        let prepared = PreparedRuntime::prepare(config, RuntimeRevision(2)).unwrap();
        let active = coordinator
            .bind_and_activate(
                RuntimeRevision(1),
                prepared,
                &TestSocketFactory,
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &Cancellation::new(),
            )
            .await
            .unwrap();

        assert_eq!(active.revision(), RuntimeRevision(2));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(2));
    }

    #[tokio::test]
    async fn bind_and_activate_returns_bound_candidate_when_activation_cas_loses() {
        let coordinator = RuntimeCoordinator::new(candidate(1));
        let (source, _) = crate::config::test_support::portable_example();
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .unwrap()
            .resolved;
        let prepared = PreparedRuntime::prepare(config, RuntimeRevision(2)).unwrap();
        let error = coordinator
            .bind_and_activate(
                RuntimeRevision(99),
                prepared,
                &TestSocketFactory,
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &Cancellation::new(),
            )
            .await
            .unwrap_err();

        let RuntimeReloadError::Activation(activation) = error else {
            panic!("expected activation CAS error");
        };
        assert_eq!(activation.expected(), RuntimeRevision(99));
        assert_eq!(activation.actual(), RuntimeRevision(1));
        assert_eq!(activation.into_candidate().revision(), RuntimeRevision(2));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
    }
}
