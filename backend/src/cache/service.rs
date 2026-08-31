//! CacheFacade：把 lookup、准入和底层 CacheStore 组合成稳定的缓存边界。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::dns::{CanonicalResponse, RuntimeRevision};
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
}

impl fmt::Debug for CacheFacade {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheFacade")
            .field("enabled", &self.options.enabled)
            .field("optimistic_enabled", &self.options.optimistic_enabled)
            .field("admission", &self.options.admission)
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
}

impl fmt::Debug for CacheRefreshPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheRefreshPermit")
            .field("key", &self.key)
            .field("version", &self.version)
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

    /// 同一 stale entry 只允许一个 refresh caller 获得执行权。
    pub fn try_consume(&self) -> bool {
        self.consumed
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
        Self { store, options }
    }

    pub fn options(&self) -> CacheFacadeOptions {
        self.options
    }

    pub fn store(&self) -> &Arc<dyn CacheStore> {
        &self.store
    }

    pub async fn lookup(
        &self,
        key: &CacheKey,
        deadline: crate::dns::Deadline,
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
        let now = Instant::now();
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
        CacheAdmissionPolicy, CacheFacade, CacheFacadeOptions, CacheLookup, CacheWriteRequest,
        CacheWriteResult, MemoryCacheStore,
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
