//! CacheFacade：把 lookup、准入和底层 CacheStore 组合成稳定的缓存边界。

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use crate::dns::{CancelReason, Cancellation, CanonicalResponse, RuntimeRevision};
use crate::ports::cache::{
    CacheCondition, CacheKey, CacheLoadCompletion, CacheLoadFailure, CacheLoadLease,
    CacheLoadReservation, CacheLoadWaiter, CacheRecord, CacheStore, CacheWriteOutcome,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::admission::{
    CacheAdmissionError, CacheAdmissionOutcome, CacheAdmissionPolicy, CacheAdmissionRejection,
    admit_response,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheFacadeOptions {
    pub enabled: bool,
    pub optimistic_enabled: bool,
    pub admission: CacheAdmissionPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheFacadeBuildError {
    ZeroRefreshCapacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LateCacheFinalizerBuildError {
    ZeroCapacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LateCacheFinalizerSubmitError {
    Shutdown,
    Capacity,
}

impl Default for CacheFacadeOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            optimistic_enabled: false,
            admission: CacheAdmissionPolicy::default(),
        }
    }
}

#[derive(Clone)]
pub struct CacheFacade {
    store: Arc<dyn CacheStore>,
    options: CacheFacadeOptions,
    refresh_admission: Arc<RefreshAdmission>,
}

/// 在客户端响应已经完成后执行有界 cache write 的后台 finalizer。
///
/// finalizer 不拥有请求响应权；调用方只能提交一条 typed write request，任务会在
/// shutdown 取消时停止，并等待所有已提交任务退出。容量不足时直接拒绝提交，避免
/// late result 在高并发下无限堆积。
#[derive(Clone)]
pub struct LateCacheFinalizer {
    state: Arc<LateCacheFinalizerState>,
}

struct LateCacheFinalizerState {
    permits: Arc<tokio::sync::Semaphore>,
    cancellation: Cancellation,
    active: AtomicUsize,
    idle: tokio::sync::Notify,
}

impl fmt::Debug for LateCacheFinalizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LateCacheFinalizer")
            .field("capacity", &self.state.permits.available_permits())
            .field("active", &self.state.active.load(Ordering::Acquire))
            .field("shutdown", &self.state.cancellation.is_cancelled())
            .finish()
    }
}

impl LateCacheFinalizer {
    pub fn new(capacity: usize) -> Result<Self, LateCacheFinalizerBuildError> {
        if capacity == 0 {
            return Err(LateCacheFinalizerBuildError::ZeroCapacity);
        }
        Ok(Self {
            state: Arc::new(LateCacheFinalizerState {
                permits: Arc::new(tokio::sync::Semaphore::new(capacity)),
                cancellation: Cancellation::new(),
                active: AtomicUsize::new(0),
                idle: tokio::sync::Notify::new(),
            }),
        })
    }

    pub fn active_tasks(&self) -> usize {
        self.state.active.load(Ordering::Acquire)
    }

    pub fn is_shutdown(&self) -> bool {
        self.state.cancellation.is_cancelled()
    }

    pub fn submit(
        &self,
        facade: Arc<CacheFacade>,
        request: CacheWriteRequest,
    ) -> Result<(), LateCacheFinalizerSubmitError> {
        self.submit_task(async move {
            let _ = facade.write_response(request).await;
        })
    }

    /// 在同一个有界 finalizer 中运行不阻塞客户端响应的后台任务。
    ///
    /// `submit` 继续覆盖普通 cache write；该入口供 optimistic refresh 或
    /// parallel late result 在获得最终写请求前复用同一容量和 shutdown 边界。
    pub fn submit_task<F>(&self, task: F) -> Result<(), LateCacheFinalizerSubmitError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.is_shutdown() {
            return Err(LateCacheFinalizerSubmitError::Shutdown);
        }
        let permit = self
            .state
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| LateCacheFinalizerSubmitError::Capacity)?;
        self.state.active.fetch_add(1, Ordering::AcqRel);
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = state.cancellation.cancelled() => {}
                _ = task => {}
            }
            drop(permit);
            state.active.fetch_sub(1, Ordering::AcqRel);
            state.idle.notify_waiters();
        });
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.state.cancellation.cancel(CancelReason::Shutdown);
        while self.active_tasks() != 0 {
            self.state.idle.notified().await;
        }
    }
}

struct RefreshAdmission {
    max_concurrency: usize,
    in_flight: AtomicUsize,
}

impl RefreshAdmission {
    const UNBOUNDED: usize = usize::MAX;

    fn new(max_concurrency: Option<usize>) -> Result<Self, CacheFacadeBuildError> {
        let max_concurrency = match max_concurrency {
            Some(0) => return Err(CacheFacadeBuildError::ZeroRefreshCapacity),
            Some(value) => value,
            None => Self::UNBOUNDED,
        };
        Ok(Self {
            max_concurrency,
            in_flight: AtomicUsize::new(0),
        })
    }

    fn try_acquire(self: &Arc<Self>) -> Option<Arc<RefreshLease>> {
        let mut current = self.in_flight.load(Ordering::Acquire);
        loop {
            if current >= self.max_concurrency || current == usize::MAX {
                return None;
            }
            match self.in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Arc::new(RefreshLease {
                        admission: Arc::clone(self),
                    }));
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn max_concurrency(&self) -> Option<usize> {
        (self.max_concurrency != Self::UNBOUNDED).then_some(self.max_concurrency)
    }
}

struct RefreshLease {
    admission: Arc<RefreshAdmission>,
}

impl Drop for RefreshLease {
    fn drop(&mut self) {
        self.admission.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl fmt::Debug for CacheFacade {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheFacade")
            .field("enabled", &self.options.enabled)
            .field("optimistic_enabled", &self.options.optimistic_enabled)
            .field("admission", &self.options.admission)
            .field(
                "refresh_capacity",
                &self.refresh_admission.max_concurrency(),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum CacheLookup {
    Disabled,
    Miss,
    Fresh(CacheRecord),
    Stale {
        record: CacheRecord,
        refresh: CacheRefreshPermit,
    },
    StoreUnavailable,
}

pub struct CacheWriteRequest {
    pub key: CacheKey,
    pub condition: CacheCondition,
    pub response: Arc<CanonicalResponse>,
    pub now: Instant,
    pub producer_revision: RuntimeRevision,
    pub format_version: u16,
    pub deadline: crate::dns::Deadline,
}

#[derive(Clone)]
pub struct CacheRefreshPermit {
    key: CacheKey,
    version: crate::ports::cache::CacheVersion,
    consumed: Arc<AtomicBool>,
    lease: Option<Arc<RefreshLease>>,
}

impl fmt::Debug for CacheRefreshPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheRefreshPermit")
            .field("key", &self.key)
            .field("version", &self.version)
            .field("admitted", &self.is_admitted())
            .field("consumed", &self.consumed.load(Ordering::Acquire))
            .finish()
    }
}

impl CacheRefreshPermit {
    pub fn key(&self) -> &CacheKey {
        &self.key
    }

    pub const fn version(&self) -> crate::ports::cache::CacheVersion {
        self.version
    }

    /// 当前 stale lookup 是否获得了 refresh admission slot。
    pub fn is_admitted(&self) -> bool {
        self.lease.is_some()
    }

    /// 同一个 permit 只允许一个 refresh caller 获得执行权。
    pub fn try_consume(&self) -> bool {
        self.is_admitted()
            && self
                .consumed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
}

#[derive(Debug)]
pub enum CacheWriteResult {
    Stored(CacheWriteOutcome),
    Rejected(CacheAdmissionRejection),
}

#[derive(Debug)]
pub enum CacheFacadeError {
    Admission(CacheAdmissionError),
    Store(PortError),
}

impl From<CacheAdmissionError> for CacheFacadeError {
    fn from(error: CacheAdmissionError) -> Self {
        Self::Admission(error)
    }
}

impl CacheFacade {
    pub fn new(store: Arc<dyn CacheStore>, options: CacheFacadeOptions) -> Self {
        Self {
            store,
            options,
            refresh_admission: Arc::new(
                RefreshAdmission::new(None)
                    .expect("unbounded refresh admission must always be valid"),
            ),
        }
    }

    pub fn try_new_with_refresh_capacity(
        store: Arc<dyn CacheStore>,
        options: CacheFacadeOptions,
        max_concurrency: usize,
    ) -> Result<Self, CacheFacadeBuildError> {
        Ok(Self {
            store,
            options,
            refresh_admission: Arc::new(RefreshAdmission::new(Some(max_concurrency))?),
        })
    }

    pub fn options(&self) -> CacheFacadeOptions {
        self.options
    }

    pub fn store(&self) -> &Arc<dyn CacheStore> {
        &self.store
    }

    pub fn refresh_capacity(&self) -> Option<usize> {
        self.refresh_admission.max_concurrency()
    }

    pub async fn lookup(
        &self,
        key: &CacheKey,
        deadline: crate::dns::Deadline,
    ) -> Result<CacheLookup, CacheFacadeError> {
        self.lookup_at(key, deadline, Instant::now()).await
    }

    pub async fn lookup_at(
        &self,
        key: &CacheKey,
        deadline: crate::dns::Deadline,
        now: Instant,
    ) -> Result<CacheLookup, CacheFacadeError> {
        if !self.options.enabled {
            return Ok(CacheLookup::Disabled);
        }
        let record = match self.store.get(key, deadline).await {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(CacheLookup::Miss),
            Err(error) if matches!(error.class(), PortErrorClass::Unavailable) => {
                return Ok(CacheLookup::StoreUnavailable);
            }
            Err(error) => return Err(CacheFacadeError::Store(error)),
        };
        if now < record.entry.expires_at {
            return Ok(CacheLookup::Fresh(record));
        }
        if self.options.optimistic_enabled
            && record
                .entry
                .stale_until
                .is_some_and(|stale_until| now < stale_until)
        {
            return Ok(CacheLookup::Stale {
                refresh: CacheRefreshPermit {
                    key: key.clone(),
                    version: record.version,
                    consumed: Arc::new(AtomicBool::new(false)),
                    lease: self.refresh_admission.try_acquire(),
                },
                record,
            });
        }
        Ok(CacheLookup::Miss)
    }

    pub fn write_response<'a>(
        &'a self,
        request: CacheWriteRequest,
    ) -> PortFuture<'a, Result<CacheWriteResult, CacheFacadeError>> {
        Box::pin(async move {
            if !self.options.enabled {
                return Ok(CacheWriteResult::Rejected(
                    CacheAdmissionRejection::OtherResponse,
                ));
            }
            let entry = match admit_response(
                self.options.admission,
                request.response,
                request.now,
                request.producer_revision,
                request.format_version,
            )? {
                CacheAdmissionOutcome::Accepted(entry) => entry,
                CacheAdmissionOutcome::Rejected(rejection) => {
                    return Ok(CacheWriteResult::Rejected(rejection));
                }
            };
            self.store
                .compare_and_swap(request.key, request.condition, entry, request.deadline)
                .await
                .map(CacheWriteResult::Stored)
                .map_err(CacheFacadeError::Store)
        })
    }

    pub fn reserve_load<'a>(
        &'a self,
        key: CacheKey,
        deadline: crate::dns::Deadline,
    ) -> PortFuture<'a, Result<CacheLoadReservation, CacheFacadeError>> {
        Box::pin(async move {
            if !self.options.enabled {
                return Err(CacheFacadeError::Store(PortError::new(
                    PortErrorClass::Unavailable,
                    "cache_facade.reserve_load",
                )));
            }
            self.store
                .reserve_load(key, deadline)
                .await
                .map_err(CacheFacadeError::Store)
        })
    }

    pub fn publish_load<'a>(
        &'a self,
        lease: CacheLoadLease,
        completion: CacheLoadCompletion,
        deadline: crate::dns::Deadline,
    ) -> PortFuture<'a, Result<(), CacheFacadeError>> {
        Box::pin(async move {
            self.store
                .publish_load(lease, completion, deadline)
                .await
                .map_err(CacheFacadeError::Store)
        })
    }

    pub fn abandon_load<'a>(
        &'a self,
        lease: CacheLoadLease,
        failure: CacheLoadFailure,
        deadline: crate::dns::Deadline,
    ) -> PortFuture<'a, Result<(), CacheFacadeError>> {
        Box::pin(async move {
            self.store
                .abandon_load(lease, failure, deadline)
                .await
                .map_err(CacheFacadeError::Store)
        })
    }

    pub fn wait_load<'a>(
        &'a self,
        waiter: CacheLoadWaiter,
        deadline: crate::dns::Deadline,
        cancellation: &'a crate::dns::Cancellation,
    ) -> PortFuture<'a, Result<CacheLoadCompletion, CacheFacadeError>> {
        Box::pin(async move {
            self.store
                .wait_load(waiter, deadline, cancellation)
                .await
                .map_err(CacheFacadeError::Store)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use crate::cache::{
        CacheAdmissionPolicy, CacheFacade, CacheFacadeBuildError, CacheFacadeOptions, CacheLookup,
        CacheWriteRequest, CacheWriteResult, LateCacheFinalizer, LateCacheFinalizerBuildError,
        LateCacheFinalizerSubmitError, MemoryCacheStore,
    };
    use crate::dns::{CanonicalQuery, CanonicalResponse, Deadline, RuntimeRevision};
    use crate::ports::cache::{CacheCondition, CacheKey, CacheNamespace, CacheStore};

    fn key() -> CacheKey {
        CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(&b"facade.example/A"[..]),
            format_version: 1,
        }
    }

    fn response(code: ResponseCode) -> CanonicalResponse {
        let mut query_message = Message::new(1, MessageType::Query, OpCode::Query);
        query_message.add_query(Query::query(
            Name::from_str("facade.example.").unwrap(),
            RecordType::A,
        ));
        let query = CanonicalQuery::from_message(query_message).unwrap();
        CanonicalResponse::empty_response(&query, code).unwrap()
    }

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(30))
    }

    #[tokio::test]
    async fn lookup_reports_disabled_and_fresh_states() {
        let store = Arc::new(MemoryCacheStore::default());
        let disabled = CacheFacade::new(
            store.clone(),
            CacheFacadeOptions {
                enabled: false,
                ..CacheFacadeOptions::default()
            },
        );
        assert!(matches!(
            disabled.lookup(&key(), deadline()).await.unwrap(),
            CacheLookup::Disabled
        ));

        let facade = CacheFacade::new(store, CacheFacadeOptions::default());
        let write = facade
            .write_response(CacheWriteRequest {
                key: key(),
                condition: CacheCondition::Absent,
                response: Arc::new(response(ResponseCode::NXDomain)),
                now: Instant::now(),
                producer_revision: RuntimeRevision(1),
                format_version: 1,
                deadline: deadline(),
            })
            .await
            .unwrap();
        assert!(matches!(write, CacheWriteResult::Stored(_)));
        assert!(matches!(
            facade.lookup(&key(), deadline()).await.unwrap(),
            CacheLookup::Fresh(_)
        ));
    }

    #[tokio::test]
    async fn stale_lookup_requires_optimistic_option_and_uses_one_shot_permit() {
        let store = Arc::new(MemoryCacheStore::default());
        let facade = CacheFacade::new(
            store,
            CacheFacadeOptions {
                optimistic_enabled: true,
                admission: CacheAdmissionPolicy::new(
                    Duration::from_secs(5),
                    Some(Duration::from_secs(30)),
                ),
                ..CacheFacadeOptions::default()
            },
        );
        let now = Instant::now();
        let entry = Arc::new(crate::ports::cache::CacheEntry {
            response: Arc::new(response(ResponseCode::NXDomain)),
            inserted_at: now - Duration::from_secs(10),
            expires_at: now - Duration::from_secs(1),
            stale_until: Some(now + Duration::from_secs(10)),
            response_class: crate::ports::cache::CacheResponseClass::NxDomain,
            producer_revision: RuntimeRevision(1),
            quality: crate::ports::cache::CacheQuality::Negative,
            checksum: 1,
            format_version: 1,
        });
        facade
            .store()
            .compare_and_swap(key(), CacheCondition::Absent, entry, deadline())
            .await
            .unwrap();
        let CacheLookup::Stale { refresh, .. } = facade.lookup(&key(), deadline()).await.unwrap()
        else {
            panic!("expected stale lookup");
        };
        assert!(refresh.try_consume());
        assert!(!refresh.try_consume());
    }

    #[tokio::test]
    async fn refresh_capacity_is_bounded_and_released_when_permit_drops() {
        let store = Arc::new(MemoryCacheStore::default());
        let facade = CacheFacade::try_new_with_refresh_capacity(
            store,
            CacheFacadeOptions {
                optimistic_enabled: true,
                admission: CacheAdmissionPolicy::new(
                    Duration::from_secs(5),
                    Some(Duration::from_secs(30)),
                ),
                ..CacheFacadeOptions::default()
            },
            1,
        )
        .unwrap();
        let now = Instant::now();
        facade
            .store()
            .compare_and_swap(
                key(),
                CacheCondition::Absent,
                Arc::new(crate::ports::cache::CacheEntry {
                    response: Arc::new(response(ResponseCode::NXDomain)),
                    inserted_at: now - Duration::from_secs(10),
                    expires_at: now - Duration::from_secs(1),
                    stale_until: Some(now + Duration::from_secs(10)),
                    response_class: crate::ports::cache::CacheResponseClass::NxDomain,
                    producer_revision: RuntimeRevision(1),
                    quality: crate::ports::cache::CacheQuality::Negative,
                    checksum: 1,
                    format_version: 1,
                }),
                deadline(),
            )
            .await
            .unwrap();

        let first = match facade.lookup_at(&key(), deadline(), now).await.unwrap() {
            CacheLookup::Stale { refresh, .. } => refresh,
            other => panic!("expected stale lookup, got {other:?}"),
        };
        assert!(first.is_admitted());
        assert!(first.try_consume());

        let second = match facade.lookup_at(&key(), deadline(), now).await.unwrap() {
            CacheLookup::Stale { refresh, .. } => refresh,
            other => panic!("expected stale lookup, got {other:?}"),
        };
        assert!(!second.is_admitted());
        assert!(!second.try_consume());

        drop(first);
        let third = match facade.lookup_at(&key(), deadline(), now).await.unwrap() {
            CacheLookup::Stale { refresh, .. } => refresh,
            other => panic!("expected stale lookup, got {other:?}"),
        };
        assert!(third.is_admitted());
        assert!(third.try_consume());
    }

    #[test]
    fn zero_refresh_capacity_is_rejected() {
        let error = CacheFacade::try_new_with_refresh_capacity(
            Arc::new(MemoryCacheStore::default()),
            CacheFacadeOptions::default(),
            0,
        )
        .unwrap_err();
        assert_eq!(error, CacheFacadeBuildError::ZeroRefreshCapacity);
    }

    #[test]
    fn zero_late_finalizer_capacity_is_rejected() {
        assert_eq!(
            LateCacheFinalizer::new(0).unwrap_err(),
            LateCacheFinalizerBuildError::ZeroCapacity
        );
    }

    #[tokio::test]
    async fn late_finalizer_writes_without_blocking_and_waits_on_shutdown() {
        let store = Arc::new(MemoryCacheStore::default());
        let facade = Arc::new(CacheFacade::new(store, CacheFacadeOptions::default()));
        let finalizer = LateCacheFinalizer::new(1).unwrap();
        finalizer
            .submit(
                Arc::clone(&facade),
                CacheWriteRequest {
                    key: key(),
                    condition: CacheCondition::Absent,
                    response: Arc::new(response(ResponseCode::NXDomain)),
                    now: Instant::now(),
                    producer_revision: RuntimeRevision(7),
                    format_version: 1,
                    deadline: deadline(),
                },
            )
            .unwrap();
        while finalizer.active_tasks() != 0 {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            facade.lookup(&key(), deadline()).await.unwrap(),
            CacheLookup::Fresh(_)
        ));

        finalizer.shutdown().await;
        assert!(finalizer.is_shutdown());
        assert_eq!(finalizer.active_tasks(), 0);
        let error = finalizer
            .submit(
                facade,
                CacheWriteRequest {
                    key: key(),
                    condition: CacheCondition::Absent,
                    response: Arc::new(response(ResponseCode::NXDomain)),
                    now: Instant::now(),
                    producer_revision: RuntimeRevision(8),
                    format_version: 1,
                    deadline: deadline(),
                },
            )
            .unwrap_err();
        assert_eq!(error, LateCacheFinalizerSubmitError::Shutdown);
    }

    #[tokio::test]
    async fn unavailable_store_is_a_degraded_lookup_state() {
        let store = Arc::new(MemoryCacheStore::default());
        store.shutdown(deadline()).await.unwrap();
        let facade = CacheFacade::new(store, CacheFacadeOptions::default());
        assert!(matches!(
            facade.lookup(&key(), deadline()).await.unwrap(),
            CacheLookup::StoreUnavailable
        ));
    }
}
