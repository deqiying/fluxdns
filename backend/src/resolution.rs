//! 进程级 DNS 解析完成事件 dispatcher 与异步消费者。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cache::CacheCommitOutcome;
use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::observability::TelemetryWriter;
use crate::ports::observation::{
    ResolutionEnvelope, ResolutionEvent, ResolutionEventSink, ResolutionPublishDisposition,
};
use crate::ports::storage::StatsDimension;
use crate::ports::telemetry::{
    Component, ComponentHealthEvent, ComponentHealthState, ConfiguredIdKind, HealthSink,
    configured_id_from_validated,
};
use crate::ports::{PortError, PortErrorClass};
use crate::storage::{
    ResolveDetailRecord, SqliteResolveDetailWriter, StatsPersistenceWorker, day_utc,
};

pub const DEFAULT_RESOLUTION_INGRESS_CAPACITY: usize = 1_024;
pub const DEFAULT_CACHE_COMMIT_QUEUE_CAPACITY: usize = 1_024;
pub const DEFAULT_DETAIL_PROJECTION_QUEUE_CAPACITY: usize = 1_024;
pub const DEFAULT_CACHE_COMMIT_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResolutionPipelineSnapshot {
    pub accepted: u64,
    pub dropped: u64,
    pub gap_started_at_utc_millis: Option<u64>,
    pub cache_commit_stored: u64,
    pub cache_commit_rejected: u64,
    pub cache_commit_conflict: u64,
    pub cache_commit_unavailable: u64,
    pub cache_commit_dropped: u64,
    pub detail_accepted: u64,
    pub detail_dropped: u64,
    pub detail_failed: u64,
}

#[derive(Debug, Default)]
pub struct ResolutionPipelineMetrics {
    accepted: AtomicU64,
    dropped: AtomicU64,
    gap_started_at_utc_millis: AtomicU64,
    cache_commit_stored: AtomicU64,
    cache_commit_rejected: AtomicU64,
    cache_commit_conflict: AtomicU64,
    cache_commit_unavailable: AtomicU64,
    cache_commit_dropped: AtomicU64,
    detail_accepted: AtomicU64,
    detail_dropped: AtomicU64,
    detail_failed: AtomicU64,
}

impl ResolutionPipelineMetrics {
    pub fn snapshot(&self) -> ResolutionPipelineSnapshot {
        let gap = self.gap_started_at_utc_millis.load(Ordering::Relaxed);
        ResolutionPipelineSnapshot {
            accepted: self.accepted.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            gap_started_at_utc_millis: (gap != 0).then(|| gap - 1),
            cache_commit_stored: self.cache_commit_stored.load(Ordering::Relaxed),
            cache_commit_rejected: self.cache_commit_rejected.load(Ordering::Relaxed),
            cache_commit_conflict: self.cache_commit_conflict.load(Ordering::Relaxed),
            cache_commit_unavailable: self.cache_commit_unavailable.load(Ordering::Relaxed),
            cache_commit_dropped: self.cache_commit_dropped.load(Ordering::Relaxed),
            detail_accepted: self.detail_accepted.load(Ordering::Relaxed),
            detail_dropped: self.detail_dropped.load(Ordering::Relaxed),
            detail_failed: self.detail_failed.load(Ordering::Relaxed),
        }
    }

    fn record_gap(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX - 1))
            .unwrap_or(0);
        let _ = self.gap_started_at_utc_millis.compare_exchange(
            0,
            millis.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    fn record_commit(&self, outcome: CacheCommitOutcome) {
        let counter = match outcome {
            CacheCommitOutcome::Stored => &self.cache_commit_stored,
            CacheCommitOutcome::Rejected => &self.cache_commit_rejected,
            CacheCommitOutcome::Conflict => &self.cache_commit_conflict,
            CacheCommitOutcome::Unavailable => &self.cache_commit_unavailable,
            CacheCommitOutcome::Dropped => &self.cache_commit_dropped,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct ResolutionPublisher {
    sender: mpsc::Sender<ResolutionEnvelope>,
    accepting: AtomicBool,
    detail_enabled: bool,
    metrics: Arc<ResolutionPipelineMetrics>,
}

impl ResolutionPublisher {
    fn stop(&self) {
        self.accepting.store(false, Ordering::Release);
    }
}

impl ResolutionEventSink for ResolutionPublisher {
    fn try_publish(
        &self,
        envelope: ResolutionEnvelope,
    ) -> Result<ResolutionPublishDisposition, PortError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Ok(ResolutionPublishDisposition::Disabled);
        }
        match self.sender.try_send(envelope) {
            Ok(()) => {
                self.metrics.accepted.fetch_add(1, Ordering::Relaxed);
                Ok(ResolutionPublishDisposition::Accepted)
            }
            Err(mpsc::error::TrySendError::Full(envelope)) => {
                self.metrics.record_gap();
                if envelope.cache_commit.is_some() {
                    self.metrics.record_commit(CacheCommitOutcome::Dropped);
                }
                Ok(ResolutionPublishDisposition::DroppedQueueFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(PortError::new(
                PortErrorClass::Unavailable,
                "resolution_event.publish",
            )),
        }
    }

    fn detail_enabled(&self) -> bool {
        self.detail_enabled
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResolutionPipelineShutdownSummary {
    pub completed: bool,
    pub snapshot: ResolutionPipelineSnapshot,
}

/// 管理进程级 ingress、cache commit 和详情 projection 三条有界队列。
pub struct ResolutionRuntime {
    publisher: Arc<ResolutionPublisher>,
    metrics: Arc<ResolutionPipelineMetrics>,
    cancellation: Cancellation,
    dispatcher: Option<JoinHandle<()>>,
    cache_worker: Option<JoinHandle<()>>,
    detail_worker: Option<JoinHandle<()>>,
}

impl ResolutionRuntime {
    pub fn start(
        stats: Arc<StatsPersistenceWorker>,
        detail_writer: Option<SqliteResolveDetailWriter>,
        telemetry: Option<Arc<TelemetryWriter>>,
    ) -> Self {
        Self::start_with_metrics(
            stats,
            detail_writer,
            telemetry,
            Arc::new(ResolutionPipelineMetrics::default()),
        )
    }

    pub fn start_with_metrics(
        stats: Arc<StatsPersistenceWorker>,
        detail_writer: Option<SqliteResolveDetailWriter>,
        telemetry: Option<Arc<TelemetryWriter>>,
        metrics: Arc<ResolutionPipelineMetrics>,
    ) -> Self {
        let (ingress_tx, ingress_rx) = mpsc::channel(DEFAULT_RESOLUTION_INGRESS_CAPACITY);
        let (cache_tx, cache_rx) = mpsc::channel(DEFAULT_CACHE_COMMIT_QUEUE_CAPACITY);
        let detail_enabled = detail_writer.is_some();
        let (detail_tx, detail_rx) = mpsc::channel(DEFAULT_DETAIL_PROJECTION_QUEUE_CAPACITY);
        let cancellation = Cancellation::new();
        let publisher = Arc::new(ResolutionPublisher {
            sender: ingress_tx,
            accepting: AtomicBool::new(true),
            detail_enabled,
            metrics: Arc::clone(&metrics),
        });
        let dispatcher = tokio::spawn(run_dispatcher(
            ingress_rx,
            cache_tx,
            detail_enabled.then_some(detail_tx),
            stats,
            Arc::clone(&metrics),
            telemetry,
            cancellation.clone(),
        ));
        let cache_worker = tokio::spawn(run_cache_worker(cache_rx, Arc::clone(&metrics)));
        let detail_worker = detail_writer.map(|writer| {
            tokio::spawn(run_detail_projector(
                detail_rx,
                writer,
                Arc::clone(&metrics),
            ))
        });
        Self {
            publisher,
            metrics,
            cancellation,
            dispatcher: Some(dispatcher),
            cache_worker: Some(cache_worker),
            detail_worker,
        }
    }

    pub fn publisher(&self) -> Arc<dyn ResolutionEventSink> {
        Arc::clone(&self.publisher) as Arc<dyn ResolutionEventSink>
    }

    pub fn metrics(&self) -> Arc<ResolutionPipelineMetrics> {
        Arc::clone(&self.metrics)
    }

    pub async fn shutdown(&mut self, deadline: Deadline) -> ResolutionPipelineShutdownSummary {
        self.publisher.stop();
        self.cancellation.cancel(CancelReason::Shutdown);
        let mut completed = true;
        for handle in [
            &mut self.dispatcher,
            &mut self.cache_worker,
            &mut self.detail_worker,
        ] {
            let Some(mut handle) = handle.take() else {
                continue;
            };
            match tokio::time::timeout(deadline.remaining(Instant::now()), &mut handle).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => completed = false,
                Err(_) => {
                    handle.abort();
                    completed = false;
                }
            }
        }
        ResolutionPipelineShutdownSummary {
            completed,
            snapshot: self.metrics.snapshot(),
        }
    }
}

async fn run_dispatcher(
    mut ingress: mpsc::Receiver<ResolutionEnvelope>,
    cache: mpsc::Sender<crate::cache::CacheCommitCandidate>,
    detail: Option<mpsc::Sender<Arc<ResolutionEvent>>>,
    stats: Arc<StatsPersistenceWorker>,
    metrics: Arc<ResolutionPipelineMetrics>,
    telemetry: Option<Arc<TelemetryWriter>>,
    cancellation: Cancellation,
) {
    let mut reported_drops = 0;
    let mut degraded = false;
    loop {
        let envelope = tokio::select! {
            _ = cancellation.cancelled() => {
                ingress.close();
                ingress.recv().await
            }
            value = ingress.recv() => value,
        };
        let Some(envelope) = envelope else {
            break;
        };
        let ResolutionEnvelope {
            event,
            cache_commit,
        } = envelope;
        if let Some(candidate) = cache_commit
            && cache.try_send(candidate).is_err()
        {
            metrics.record_commit(CacheCommitOutcome::Dropped);
        }
        record_stats(&stats, &event);
        if let Some(telemetry) = &telemetry {
            telemetry.record_resolution(&event);
        }
        if let Some(detail) = &detail
            && event.detail.is_some()
        {
            match detail.try_send(Arc::clone(&event)) {
                Ok(()) => {
                    metrics.detail_accepted.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    metrics.detail_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let dropped = metrics.dropped.load(Ordering::Relaxed);
        if dropped > reported_drops {
            reported_drops = dropped;
            degraded = true;
            publish_health(
                telemetry.as_deref(),
                ComponentHealthState::Degraded,
                dropped,
                true,
                Some("resolution event ingress gap"),
            );
        } else if degraded {
            degraded = false;
            publish_health(
                telemetry.as_deref(),
                ComponentHealthState::Healthy,
                dropped,
                false,
                None,
            );
        }
    }
}

async fn run_cache_worker(
    mut receiver: mpsc::Receiver<crate::cache::CacheCommitCandidate>,
    metrics: Arc<ResolutionPipelineMetrics>,
) {
    while let Some(candidate) = receiver.recv().await {
        let outcome = candidate.commit(DEFAULT_CACHE_COMMIT_TIMEOUT).await;
        metrics.record_commit(outcome);
    }
}

async fn run_detail_projector(
    mut receiver: mpsc::Receiver<Arc<ResolutionEvent>>,
    writer: SqliteResolveDetailWriter,
    metrics: Arc<ResolutionPipelineMetrics>,
) {
    while let Some(event) = receiver.recv().await {
        match ResolveDetailRecord::from_resolution_event(&event) {
            Ok(record) => match writer.try_write(record) {
                Ok(()) => {}
                Err(error) if matches!(error.class(), PortErrorClass::ResourceExhausted) => {
                    metrics.detail_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    metrics.detail_failed.fetch_add(1, Ordering::Relaxed);
                }
            },
            Err(_) => {
                metrics.detail_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn record_stats(worker: &StatsPersistenceWorker, event: &ResolutionEvent) {
    let Ok(day) = day_utc(event.occurred_at) else {
        return;
    };
    let mut dimensions = vec![
        StatsDimension::transport(event.transport),
        StatsDimension::attempt_outcome(event.outcome),
        StatsDimension::source(event.source),
        StatsDimension::cache_status(event.cache_lookup_status),
    ];
    if let crate::ports::observation::ResolutionTerminal::Response { rcode, .. } = event.terminal {
        dimensions.push(StatsDimension::rcode(rcode));
    }
    if let Some(id) = event
        .client_bucket
        .as_deref()
        .and_then(|id| configured_id_from_validated(ConfiguredIdKind::ClientBucket, id))
        && let Ok(dimension) = StatsDimension::client_bucket(id)
    {
        dimensions.push(dimension);
    }
    if let Some(id) = event
        .strategy_id
        .as_deref()
        .and_then(|id| configured_id_from_validated(ConfiguredIdKind::Strategy, id))
        && let Ok(dimension) = StatsDimension::strategy(id)
    {
        dimensions.push(dimension);
    }
    if let Some(id) = event
        .upstream_used_id
        .as_deref()
        .or(event.upstream_member_id.as_deref())
        .or(event.upstream_id.as_deref())
        .and_then(|id| configured_id_from_validated(ConfiguredIdKind::Upstream, id))
        && let Ok(dimension) = StatsDimension::upstream(id)
    {
        dimensions.push(dimension);
    }
    let _ = worker.record_request(day, dimensions);
}

fn publish_health(
    telemetry: Option<&TelemetryWriter>,
    state: ComponentHealthState,
    retry_count: u64,
    gap: bool,
    reason: Option<&'static str>,
) {
    let Some(telemetry) = telemetry else {
        return;
    };
    let now = Instant::now();
    let _ = HealthSink::update(
        telemetry,
        ComponentHealthEvent {
            component: Component::Resolution,
            state,
            first_seen: now,
            last_changed: now,
            last_success: (state == ComponentHealthState::Healthy).then_some(now),
            retry_count,
            stale_age_micros: None,
            persistence_gap: gap,
            safe_reason: reason,
        },
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::SystemTime;

    use tokio::sync::mpsc;

    use crate::cache::CacheCommitOutcome;
    use crate::dns::{ResponseClass, RuntimeRevision, TransportClass};
    use crate::ports::observation::{
        ResolutionEnvelope, ResolutionEvent, ResolutionEventSink, ResolutionPublishDisposition,
        ResolutionTerminal,
    };
    use crate::ports::storage::StatsSource;
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};

    use super::{ResolutionPipelineMetrics, ResolutionPublisher};

    fn event() -> Arc<ResolutionEvent> {
        Arc::new(ResolutionEvent {
            occurred_at: SystemTime::now(),
            duration_millis: 8,
            dns_core_duration_micros: 250,
            listener_id: Arc::from("dns"),
            route_id: None,
            client_bucket: None,
            strategy_id: None,
            upstream_id: None,
            upstream_member_id: None,
            upstream_used_id: None,
            matched_rule_source: None,
            matched_resource_id: None,
            matched_rule_ordinal: None,
            resource_version: None,
            transport: TransportClass::Datagram,
            terminal: ResolutionTerminal::Response {
                class: ResponseClass::NoData,
                rcode: 0,
            },
            outcome: OutcomeClass::Success,
            source: StatsSource::Upstream,
            cache_lookup_status: CacheStatus::Miss,
            runtime_revision: RuntimeRevision(1),
            detail: None,
        })
    }

    #[tokio::test]
    async fn dispatcher_aggregates_bounded_latency_and_results_without_detail_collection() {
        use crate::dns::Deadline;
        use crate::observability::{StructuredTelemetryOutput, TelemetryWriter};
        use crate::ports::telemetry::{
            Component, EventName, LogEvent, LogLevel, LogSink, MetricName, MetricValue,
        };
        use std::time::{Duration, Instant};
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stats = Arc::new(crate::storage::StatsPersistenceWorker::new(Arc::new(
            crate::storage::InMemoryStorageBackend::new(),
        )));
        let writer = Arc::new(
            TelemetryWriter::new(
                1,
                Arc::new(StructuredTelemetryOutput::from_writer(Box::new(Capture(
                    Arc::clone(&output),
                )))),
            )
            .unwrap(),
        );
        let mut runtime = super::ResolutionRuntime::start(stats, None, Some(Arc::clone(&writer)));
        let publisher = runtime.publisher();
        assert!(!publisher.detail_enabled());
        writer
            .emit(LogEvent {
                occurred_at: SystemTime::now(),
                level: LogLevel::Warn,
                name: EventName::parse("test_congestion").unwrap(),
                component: Component::Resolution,
                request_digest: None,
                configured_id: None,
                outcome: OutcomeClass::Success,
                runtime_revision: None,
                message: "test",
            })
            .unwrap();
        for (duration, outcome, cache) in [
            (0, OutcomeClass::Success, CacheStatus::Fresh),
            (5, OutcomeClass::Timeout, CacheStatus::Miss),
            (50, OutcomeClass::Success, CacheStatus::Miss),
        ] {
            let mut event = (*event()).clone();
            event.duration_millis = duration;
            event.outcome = outcome;
            event.cache_lookup_status = cache;
            event.listener_id = Arc::from("private-listener");
            event.client_bucket = Some(Arc::from("private-client"));
            assert_eq!(
                publisher
                    .try_publish(ResolutionEnvelope {
                        event: Arc::new(event),
                        cache_commit: None
                    })
                    .unwrap(),
                ResolutionPublishDisposition::Accepted
            );
        }
        let summary = runtime
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .await;
        assert!(summary.completed);
        assert_eq!(summary.snapshot.accepted, 3);
        assert_eq!(summary.snapshot.detail_accepted, 0);
        let snapshot = writer.metric_snapshot();
        assert_eq!(snapshot.len(), 14);
        let histogram = snapshot
            .iter()
            .find(|item| item.name == MetricName::RequestLatency)
            .unwrap()
            .histogram
            .unwrap();
        assert_eq!(histogram.count, 3);
        assert_eq!(histogram.sum_micros, 55_000);
        assert_eq!(histogram.buckets[0], 1);
        assert_eq!(histogram.buckets[1], 2);
        assert_eq!(histogram.buckets[4], 3);
        assert_eq!(histogram.buckets[11], 3);
        assert!(
            snapshot
                .iter()
                .any(|item| item.name == MetricName::RequestsTotal
                    && item.outcome == Some(OutcomeClass::Timeout)
                    && item.value == MetricValue::Counter(1))
        );
        assert!(
            snapshot
                .iter()
                .any(|item| item.name == MetricName::CacheOperations
                    && item.cache_status == Some(CacheStatus::Miss)
                    && item.value == MetricValue::Counter(2))
        );
        assert!(!format!("{snapshot:?}").contains("private"));
        assert_eq!(
            writer.stats().pending(),
            1,
            "request metrics must not join the full log queue"
        );
        let mut overflowing = (*event()).clone();
        overflowing.dns_core_duration_micros = u64::MAX;
        writer.record_resolution(&overflowing);
        assert_eq!(
            writer.metric_snapshot(),
            snapshot,
            "a rejected event must not partially update other counters"
        );
        writer
            .shutdown(Deadline::new(Instant::now() + Duration::from_secs(1)))
            .unwrap();
        writer.record_resolution(&event());
        assert_eq!(writer.metric_snapshot(), snapshot);
        assert_eq!(writer.stats().rejected_metrics(), 2);
        let encoded = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(!encoded.contains("private"));
        let json: Vec<serde_json::Value> = encoded
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let metric = json
            .iter()
            .find(|item| item["name"] == "requestlatency")
            .unwrap();
        assert_eq!(metric["temporality"], "cumulative");
        assert_eq!(metric["histogram"]["count"], 3);
        assert_eq!(metric["histogram"]["sum"], 55_000);
        assert_eq!(metric["histogram"]["buckets"][11]["le"], "+Inf");
    }

    #[test]
    fn bounded_publisher_drops_without_waiting_and_records_a_gap() {
        let (sender, _receiver) = mpsc::channel(1);
        let metrics = Arc::new(ResolutionPipelineMetrics::default());
        let publisher = ResolutionPublisher {
            sender,
            accepting: AtomicBool::new(true),
            detail_enabled: false,
            metrics: Arc::clone(&metrics),
        };

        assert_eq!(
            publisher
                .try_publish(ResolutionEnvelope {
                    event: event(),
                    cache_commit: None,
                })
                .unwrap(),
            ResolutionPublishDisposition::Accepted
        );
        assert_eq!(
            publisher
                .try_publish(ResolutionEnvelope {
                    event: event(),
                    cache_commit: None,
                })
                .unwrap(),
            ResolutionPublishDisposition::DroppedQueueFull
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.accepted, 1);
        assert_eq!(snapshot.dropped, 1);
        assert!(snapshot.gap_started_at_utc_millis.is_some());
        assert!(!publisher.detail_enabled());

        publisher.stop();
        assert_eq!(
            publisher
                .try_publish(ResolutionEnvelope {
                    event: event(),
                    cache_commit: None,
                })
                .unwrap(),
            ResolutionPublishDisposition::Disabled
        );
        assert_eq!(metrics.snapshot().dropped, 1);
    }

    #[test]
    fn cache_commit_outcomes_are_counted_independently_from_lookup_status() {
        let metrics = ResolutionPipelineMetrics::default();
        for outcome in [
            CacheCommitOutcome::Stored,
            CacheCommitOutcome::Rejected,
            CacheCommitOutcome::Conflict,
            CacheCommitOutcome::Unavailable,
            CacheCommitOutcome::Dropped,
        ] {
            metrics.record_commit(outcome);
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.cache_commit_stored, 1);
        assert_eq!(snapshot.cache_commit_rejected, 1);
        assert_eq!(snapshot.cache_commit_conflict, 1);
        assert_eq!(snapshot.cache_commit_unavailable, 1);
        assert_eq!(snapshot.cache_commit_dropped, 1);
        assert_eq!(snapshot.accepted, 0);
        assert_eq!(snapshot.dropped, 0);
        assert_eq!(snapshot.gap_started_at_utc_millis, None);
    }
}
