//! 内存分析单槽采集器；blocking worker 无发布权，超时/取消后不能晚到覆盖。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::contract::*;
use super::events::EventHub;
use super::memory_os::{GAP_SECONDS, INTERVAL_SECONDS};
use super::metrics::MetricsOwner;
use crate::resource::{RuleMemoryHolder, analyze_rule_memory};
use crate::runtime::{ActiveRuntime, RuntimeCoordinator};

const COLLECTION_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct CollectedState {
    sequence: u64,
    snapshot: Option<MemorySnapshot>,
    previous: Option<MemorySnapshot>,
}

pub(super) struct MemoryCollector {
    slot: Arc<Semaphore>,
    state: Mutex<CollectedState>,
    failure: Mutex<Option<MemoryCollectionFailure>>,
}

impl Default for MemoryCollector {
    fn default() -> Self {
        Self {
            slot: Arc::new(Semaphore::new(1)),
            state: Mutex::new(CollectedState::default()),
            failure: Mutex::new(None),
        }
    }
}

impl MemoryCollector {
    pub(super) fn read(&self, metrics: &MetricsOwner) -> MemoryAnalysis {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.analysis(metrics, &state, None)
    }

    fn analysis(
        &self,
        metrics: &MetricsOwner,
        state: &CollectedState,
        completed: Option<MemoryCollectionState>,
    ) -> MemoryAnalysis {
        let (process_id, started_at_ms) = metrics.process_identity();
        let (os, trend) = metrics.memory_samples();
        let failure = *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        MemoryAnalysis {
            process_id,
            started_at_ms,
            sample_interval_seconds: INTERVAL_SECONDS,
            gap_after_seconds: GAP_SECONDS,
            os,
            trend,
            snapshot: state.snapshot.clone(),
            previous_snapshot: state.previous.clone(),
            collection_state: completed.unwrap_or_else(|| {
                if self.slot.available_permits() == 0 {
                    MemoryCollectionState::Collecting
                } else if failure.is_some() {
                    MemoryCollectionState::Failed
                } else {
                    MemoryCollectionState::Idle
                }
            }),
            last_failure: failure,
        }
    }

    pub(super) async fn collect(
        &self,
        coordinator: Arc<RuntimeCoordinator>,
        metrics: Arc<MetricsOwner>,
        events: Option<Arc<EventHub>>,
    ) -> Result<MemoryAnalysis, ErrorCode> {
        let runtime = coordinator.load();
        let revision = runtime.revision().0;
        let sources = metrics.memory_sources();
        let (modules, permit, deadline) = self
            .run_work(
                move |deadline| {
                    collect_modules(&runtime, sources.as_deref(), events.as_deref(), deadline)
                },
                COLLECTION_TIMEOUT,
            )
            .await?;
        self.publish(modules, permit, deadline, revision, &metrics, || {
            coordinator.load().revision().0 == revision
        })
        .await
    }

    /// 锁等待可取消且计入同一预算；只有仍存活的请求能提交快照。
    async fn publish(
        &self,
        modules: Vec<MemoryModule>,
        permit: OwnedSemaphorePermit,
        deadline: Instant,
        revision: u64,
        metrics: &MetricsOwner,
        revision_matches: impl Fn() -> bool,
    ) -> Result<MemoryAnalysis, ErrorCode> {
        loop {
            if Instant::now() >= deadline {
                self.fail(MemoryCollectionFailure::Timeout);
                return Err(ErrorCode::RequestTimeout);
            }
            // try_lock 的 guard 不跨 await，避免阻塞执行器或在取消后继续发布。
            if let Some(result) =
                self.try_publish(&modules, deadline, revision, metrics, &revision_matches)
            {
                // 成功响应已固定；释放槽位后不再读取下一次采集的状态。
                drop(permit);
                return result;
            }
            tokio::time::sleep_until(tokio::time::Instant::from_std(
                deadline.min(Instant::now() + Duration::from_millis(1)),
            ))
            .await;
        }
    }

    fn try_publish(
        &self,
        modules: &[MemoryModule],
        deadline: Instant,
        revision: u64,
        metrics: &MetricsOwner,
        revision_matches: &impl Fn() -> bool,
    ) -> Option<Result<MemoryAnalysis, ErrorCode>> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let Some(sequence) = state.sequence.checked_add(1) else {
            self.fail(MemoryCollectionFailure::SamplingFailed);
            return Some(Err(ErrorCode::ServiceUnavailable));
        };
        // 在提交前完成有界的响应拷贝，再复核 deadline/revision。
        let next = CollectedState {
            sequence,
            previous: state.snapshot.clone(),
            snapshot: Some(MemorySnapshot {
                snapshot_id: sequence.into(),
                sampled_at_ms: unix_millis(),
                runtime_revision: revision.into(),
                modules: modules.to_vec(),
            }),
        };
        let mut response = self.analysis(metrics, &next, Some(MemoryCollectionState::Idle));
        if Instant::now() >= deadline {
            self.fail(MemoryCollectionFailure::Timeout);
            return Some(Err(ErrorCode::RequestTimeout));
        }
        if !revision_matches() {
            self.fail(MemoryCollectionFailure::RuntimeChanged);
            return Some(Err(ErrorCode::ServiceUnavailable));
        }
        *state = next;
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        response.last_failure = None;
        Some(Ok(response))
    }

    /// permit 随真正的 worker 存活；超时只放弃等待，不让后台任务排队累积。
    async fn run_work<F>(
        &self,
        work: F,
        timeout: Duration,
    ) -> Result<(Vec<MemoryModule>, OwnedSemaphorePermit, Instant), ErrorCode>
    where
        F: FnOnce(Instant) -> Result<Vec<MemoryModule>, MemoryCollectionFailure> + Send + 'static,
    {
        let permit = Arc::clone(&self.slot)
            .try_acquire_owned()
            .map_err(|_| ErrorCode::RateLimited)?;
        let deadline = Instant::now() + timeout;
        let task = tokio::task::spawn_blocking(move || (work(deadline), permit));
        let outcome = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task).await;
        match outcome {
            Ok(Ok((Ok(modules), permit))) if Instant::now() < deadline => {
                Ok((modules, permit, deadline))
            }
            Ok(Ok((Err(reason), _))) => {
                self.fail(reason);
                Err(if matches!(reason, MemoryCollectionFailure::Timeout) {
                    ErrorCode::RequestTimeout
                } else {
                    ErrorCode::ServiceUnavailable
                })
            }
            Err(_) | Ok(Ok((Ok(_), _))) => {
                self.fail(MemoryCollectionFailure::Timeout);
                Err(ErrorCode::RequestTimeout)
            }
            Ok(Err(_)) => {
                self.fail(MemoryCollectionFailure::SamplingFailed);
                Err(ErrorCode::ServiceUnavailable)
            }
        }
    }

    fn fail(&self, failure: MemoryCollectionFailure) {
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(failure);
    }
}

fn collect_modules(
    runtime: &ActiveRuntime,
    sources: Option<&super::memory_sources::MemoryRuntimeSources>,
    events: Option<&EventHub>,
    deadline: Instant,
) -> Result<Vec<MemoryModule>, MemoryCollectionFailure> {
    let report = analyze_rule_memory(runtime.rule_memory_roots(), deadline).map_err(|_| {
        if Instant::now() >= deadline {
            MemoryCollectionFailure::Timeout
        } else {
            MemoryCollectionFailure::SamplingFailed
        }
    })?;
    let mut rules = module(MemoryModuleId::Rules, MemoryProbeState::Available);
    rules.estimated_bytes = Some(report.estimated_bytes.into());
    rules.estimate_version = Some("rule-payload-v1".to_owned());
    rules.counters = vec![
        counter(
            "unique_indexes",
            report.unique_indexes,
            MemoryCounterKind::Count,
        ),
        counter(
            "retained_indexes",
            report.retained_indexes,
            MemoryCounterKind::Count,
        ),
    ];
    rules.rules = report
        .rows
        .into_iter()
        .map(|row| MemoryRuleRow {
            resource: row.resource,
            selector: row.selector,
            rule_count: row.rule_count.into(),
            estimated_bytes: row.estimated_bytes.into(),
            holders: row
                .holders
                .into_iter()
                .map(|holder| {
                    match holder {
                        RuleMemoryHolder::Initial => "initial",
                        RuleMemoryHolder::Worker => "worker",
                        RuleMemoryHolder::Policy => "policy",
                    }
                    .to_owned()
                })
                .collect(),
            retained: row.retained,
        })
        .collect();
    let mut cache = module(MemoryModuleId::DnsCache, MemoryProbeState::NotCollected);
    if let Some(core) = runtime.snapshot().policy_core() {
        let (entries, weight, budget) = core.cache_snapshot_source().memory_counts();
        cache.state = MemoryProbeState::Available;
        cache
            .counters
            .push(counter("entries", entries, MemoryCounterKind::Count));
        cache.counters.push(counter(
            "weighted_size",
            weight,
            MemoryCounterKind::BillingBytes,
        ));
        if let Some(budget) = budget {
            cache.counters.push(counter(
                "max_weight",
                budget,
                MemoryCounterKind::BudgetBytes,
            ));
        }
    }
    let mut details = module(MemoryModuleId::DetailsWs, MemoryProbeState::NotCollected);
    if let Some(events) = events {
        if let Some((records, batches, bytes, connections)) = events.memory_counts() {
            details.state = MemoryProbeState::Available;
            details.counters = vec![
                counter("replay_records", records, MemoryCounterKind::Count),
                counter("replay_batches", batches, MemoryCounterKind::Count),
                counter(
                    "replay_billing_bytes",
                    bytes,
                    MemoryCounterKind::BillingBytes,
                ),
                counter("ws_connections", connections, MemoryCounterKind::Count),
            ];
        } else {
            details.state = MemoryProbeState::Busy;
        }
    }
    let mut modules = vec![rules, cache, details];
    modules.extend(sources.map_or_else(
        || {
            vec![
                module(
                    MemoryModuleId::StatisticsQueues,
                    MemoryProbeState::NotCollected,
                ),
                module(MemoryModuleId::Sqlite, MemoryProbeState::NotCollected),
            ]
        },
        |sources| sources.modules(),
    ));
    modules.push(module(
        MemoryModuleId::Allocator,
        MemoryProbeState::NotCollected,
    ));
    if Instant::now() >= deadline {
        return Err(MemoryCollectionFailure::Timeout);
    }
    Ok(modules)
}

fn module(id: MemoryModuleId, state: MemoryProbeState) -> MemoryModule {
    MemoryModule {
        id,
        state,
        estimated_bytes: None,
        estimate_version: None,
        counters: Vec::new(),
        rules: Vec::new(),
    }
}

fn counter(key: &str, value: u64, kind: MemoryCounterKind) -> MemoryCounter {
    MemoryCounter {
        key: key.to_owned(),
        value: value.into(),
        kind,
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis().min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn single_flight_keeps_slot_until_worker_really_finishes() {
        let owner = Arc::new(MemoryCollector::default());
        let gate = Arc::new(std::sync::Barrier::new(2));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let other = Arc::clone(&owner);
        let worker_gate = Arc::clone(&gate);
        let worker_started = Arc::clone(&started);
        let call = tokio::spawn(async move {
            other
                .run_work(
                    move |_| {
                        worker_started.store(true, std::sync::atomic::Ordering::Release);
                        worker_gate.wait();
                        Ok(Vec::new())
                    },
                    Duration::from_millis(40),
                )
                .await
        });
        while !started.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            call.await.unwrap(),
            Err(ErrorCode::RequestTimeout)
        ));
        assert!(matches!(
            owner
                .run_work(|_| Ok(Vec::new()), Duration::from_secs(1))
                .await,
            Err(ErrorCode::RateLimited)
        ));
        gate.wait();
        for _ in 0..100 {
            if owner.slot.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(owner.slot.available_permits(), 1);
        assert!(owner.state.lock().unwrap().snapshot.is_none());
        let (_, permit, _) = owner
            .run_work(|_| Ok(Vec::new()), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(owner.slot.available_permits(), 0);
        drop(permit);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_publish_or_release_live_worker() {
        let owner = Arc::new(MemoryCollector::default());
        let gate = Arc::new(std::sync::Barrier::new(2));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let other = Arc::clone(&owner);
        let worker_gate = Arc::clone(&gate);
        let worker_started = Arc::clone(&started);
        let call = tokio::spawn(async move {
            other
                .run_work(
                    move |_| {
                        worker_started.store(true, std::sync::atomic::Ordering::Release);
                        worker_gate.wait();
                        Ok(Vec::new())
                    },
                    Duration::from_secs(2),
                )
                .await
        });
        while !started.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        call.abort();
        let _ = call.await;
        assert_eq!(owner.slot.available_permits(), 0);
        gate.wait();
        for _ in 0..100 {
            if owner.slot.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(owner.state.lock().unwrap().snapshot.is_none());
    }

    #[tokio::test]
    async fn publication_lock_wait_is_bounded_and_never_publishes_late() {
        let owner = Arc::new(MemoryCollector::default());
        let metrics = MetricsOwner::new();
        let (modules, permit, deadline) = owner
            .run_work(|_| Ok(Vec::new()), Duration::from_millis(30))
            .await
            .unwrap();
        let other = Arc::clone(&owner);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let held = other.state.lock().unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            assert!(held.snapshot.is_none());
        });
        ready_rx.recv().unwrap();
        let result = owner
            .publish(modules, permit, deadline, 1, &metrics, || true)
            .await;
        assert!(matches!(result, Err(ErrorCode::RequestTimeout)));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(owner.state.lock().unwrap().snapshot.is_none());
        assert_eq!(owner.slot.available_permits(), 1);
        assert_eq!(
            owner.read(&metrics).last_failure,
            Some(MemoryCollectionFailure::Timeout)
        );
    }

    #[tokio::test]
    async fn completed_response_does_not_inherit_the_next_collection_state() {
        let owner = MemoryCollector::default();
        let metrics = MetricsOwner::new();
        let (modules, permit, deadline) = owner
            .run_work(|_| Ok(Vec::new()), Duration::from_secs(1))
            .await
            .unwrap();
        let response = owner
            .publish(modules, permit, deadline, 1, &metrics, || true)
            .await
            .unwrap();
        let next = owner.slot.clone().try_acquire_owned().unwrap();
        assert_eq!(
            owner.read(&metrics).collection_state,
            MemoryCollectionState::Collecting
        );
        assert_eq!(response.collection_state, MemoryCollectionState::Idle);
        assert!(response.last_failure.is_none());
        assert!(response.snapshot.is_some());
        drop(next);
    }
}
