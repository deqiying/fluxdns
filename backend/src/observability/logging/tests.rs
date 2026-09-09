use super::*;
use crate::ports::telemetry::{
    Component, ComponentHealthEvent, ComponentHealthState, EventName, HealthSink, LogEvent,
    LogLevel, LogSink, MetricEvent, MetricLabel, MetricLabelKey, MetricLabelValue, MetricName,
    MetricValue, MetricsSink, OutcomeClass,
};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime},
};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/p1-logging-tests");
        let root = base.join(format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn config(&self, enable: bool, name: &str, level: LogLevelDto) -> ResolvedLogs {
        ResolvedLogs {
            enable,
            path: self.root.join(name),
            level,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/p1-logging-tests")
            .canonicalize()
            .unwrap();
        let root = self.root.canonicalize().unwrap();
        assert_eq!(root.parent(), Some(base.as_path()));
        fs::remove_dir_all(root).unwrap();
    }
}

fn budget() -> Deadline {
    Deadline::new(Instant::now() + Duration::from_secs(5))
}

fn log(name: &str, level: LogLevel) -> LogEvent {
    LogEvent {
        occurred_at: SystemTime::now(),
        name: EventName::parse(name.to_owned()).unwrap(),
        level,
        component: Component::Application,
        request_digest: None,
        configured_id: None,
        outcome: OutcomeClass::Success,
        runtime_revision: None,
        message: "logging test",
    }
}

#[tokio::test]
async fn off_on_level_and_path_switch_keep_metrics_and_never_replay_disabled_logs() {
    let fixture = Fixture::new();
    let off = fixture.config(false, "not-created.log", LogLevelDto::Info);
    let (owner, writer, dispatch) = LoggingOwner::for_test(off.clone());
    assert!(!off.path.exists());
    writer
        .emit(log("disabled.initial", LogLevel::Error))
        .unwrap();
    writer
        .record(
            MetricEvent::new(
                MetricName::ResolutionEventsAccepted,
                vec![
                    MetricLabel::new(
                        MetricLabelKey::Component,
                        MetricLabelValue::Component(Component::Resolution),
                    )
                    .unwrap(),
                ],
                MetricValue::Counter(3),
            )
            .unwrap(),
        )
        .unwrap();
    let now = Instant::now();
    writer
        .update(ComponentHealthEvent {
            component: Component::Telemetry,
            state: ComponentHealthState::Healthy,
            first_seen: now,
            last_changed: now,
            last_success: Some(now),
            retry_count: 0,
            stale_age_micros: None,
            persistence_gap: false,
            safe_reason: None,
        })
        .unwrap();
    assert_eq!(writer.health.lock().unwrap().len(), 1);
    assert_eq!(writer.metric_snapshot().len(), 1);
    assert_eq!(writer.stats().pending(), 1);

    let on = fixture.config(true, "first.log", LogLevelDto::Info);
    owner
        .prepare(&off, on.clone(), budget())
        .await
        .unwrap()
        .publish_with(|| Ok::<_, ()>(()))
        .unwrap();
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::debug!(event = "filtered.tracing", "filtered");
        tracing::info!(event = "accepted.tracing", "accepted");
    });
    writer
        .emit(log("filtered.direct", LogLevel::Debug))
        .unwrap();
    writer.emit(log("accepted.direct", LogLevel::Info)).unwrap();
    writer.flush_now(budget()).unwrap();
    let first = fs::read_to_string(&on.path).unwrap();
    assert!(first.contains("accepted.tracing") && first.contains("accepted.direct"));
    assert!(!first.contains("filtered") && !first.contains("disabled"));
    assert!(first.contains("\"kind\":\"metric\"") && first.contains("\"kind\":\"health\""));

    let warn = fixture.config(true, "first.log", LogLevelDto::Warn);
    writer.emit(log("filtered.queued", LogLevel::Info)).unwrap();
    owner
        .prepare(&on, warn.clone(), budget())
        .await
        .unwrap()
        .publish_with(|| Ok::<_, ()>(()))
        .unwrap();
    writer.emit(log("accepted.warn", LogLevel::Warn)).unwrap();
    writer.flush_now(budget()).unwrap();
    let prior = fs::read_to_string(&on.path).unwrap();
    assert!(!prior.contains("filtered.queued"));
    assert!(prior.contains("accepted.warn"));

    writer
        .emit(log("disabled.queued", LogLevel::Error))
        .unwrap();
    owner
        .prepare(&warn, off.clone(), budget())
        .await
        .unwrap()
        .publish_with(|| Ok::<_, ()>(()))
        .unwrap();
    writer.emit(log("disabled.later", LogLevel::Error)).unwrap();
    writer.flush_now(budget()).unwrap();
    assert_eq!(fs::read_to_string(&on.path).unwrap(), prior);
    let next = fixture.config(true, "second.log", LogLevelDto::Debug);
    owner
        .prepare(&off, next.clone(), budget())
        .await
        .unwrap()
        .publish_with(|| Ok::<_, ()>(()))
        .unwrap();
    writer
        .emit(log("accepted.second", LogLevel::Debug))
        .unwrap();
    writer.flush_now(budget()).unwrap();
    let second = fs::read_to_string(&next.path).unwrap();
    assert!(second.contains("accepted.second") && !second.contains("disabled"));
    assert_eq!(fs::read_to_string(&on.path).unwrap(), prior);
    assert!(!off.path.exists());
    assert_eq!(writer.metric_snapshot().len(), 1);
    assert!(owner.matches(&next, &writer));
}

#[tokio::test]
async fn preparation_and_publication_failures_keep_previous_output_and_filter() {
    let fixture = Fixture::new();
    let initial = fixture.config(true, "old.log", LogLevelDto::Debug);
    let (owner, writer, _dispatch) = LoggingOwner::for_test(initial.clone());
    let mut bad = initial.clone();
    bad.path = fixture.root.clone();
    assert!(matches!(
        owner.prepare(&initial, bad, budget()).await,
        Err(LoggingError::Prepare(_))
    ));
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let blocked = fixture.config(true, "occupied.log", LogLevelDto::Error);
        fs::write(&blocked.path, b"external log bytes").unwrap();
        let _held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&blocked.path)
            .unwrap();
        assert!(matches!(
            owner.prepare(&initial, blocked.clone(), budget()).await,
            Err(LoggingError::Prepare(_))
        ));
        assert_eq!(fs::read(&blocked.path).unwrap(), b"external log bytes");
        assert!(owner.matches(&initial, &writer));
    }
    assert!(matches!(
        owner
            .prepare(&initial, initial.clone(), Deadline::new(Instant::now()))
            .await,
        Err(LoggingError::Timeout)
    ));
    let next = fixture.config(true, "new.log", LogLevelDto::Error);
    let prepared = owner
        .prepare(&initial, next.clone(), budget())
        .await
        .unwrap();
    assert!(matches!(
        owner.prepare(&initial, next.clone(), budget()).await,
        Err(LoggingError::Busy)
    ));
    assert!(matches!(
        prepared.publish_with(|| Err::<(), _>("runtime conflict")),
        Err(LoggingPublishError::Application("runtime conflict"))
    ));
    assert!(owner.matches(&initial, &writer));
    assert_eq!(
        owner.filter.with_current(|filter| *filter).unwrap(),
        LevelFilter::DEBUG
    );
    writer.emit(log("old.retained", LogLevel::Debug)).unwrap();
    writer.flush_now(budget()).unwrap();
    assert!(
        fs::read_to_string(&initial.path)
            .unwrap()
            .contains("old.retained")
    );
    assert_eq!(fs::read(&next.path).unwrap(), b"");

    let prepared = owner
        .prepare(&initial, next.clone(), budget())
        .await
        .unwrap();
    let flushing = writer.flush_lock.lock().unwrap();
    assert!(matches!(
        prepared.publish_with(|| panic!("must not publish while flushing")),
        Err::<(), _>(LoggingPublishError::<()>::Logging(LoggingError::Busy))
    ));
    drop(flushing);
    assert!(owner.matches(&initial, &writer));
    drop(owner.prepare(&initial, next, budget()).await.unwrap());
    assert!(owner.matches(&initial, &writer));
}

#[tokio::test]
async fn filter_failure_and_failed_compensation_are_distinct() {
    let fixture = Fixture::new();
    for rollback in [false, true] {
        let initial = fixture.config(true, &format!("old-{rollback}.log"), LogLevelDto::Info);
        let (owner, writer, dispatch) = LoggingOwner::for_test(initial.clone());
        let next = fixture.config(false, "unused.log", LogLevelDto::Error);
        let prepared = owner.prepare(&initial, next, budget()).await.unwrap();
        let result = if rollback {
            prepared.publish_with(|| {
                drop(dispatch);
                Err::<(), _>("runtime conflict")
            })
        } else {
            drop(dispatch);
            prepared.publish_with(|| panic!("missing subscriber must reject before publication"))
        };
        if rollback {
            assert!(matches!(
                result,
                Err(LoggingPublishError::CompensationFailed("runtime conflict"))
            ));
        } else {
            assert!(matches!(
                result,
                Err(LoggingPublishError::Logging(LoggingError::Filter))
            ));
        }
        assert!(owner.matches(&initial, &writer));
        writer.emit(log("old.retained", LogLevel::Info)).unwrap();
        writer.flush_now(budget()).unwrap();
        assert!(
            fs::read_to_string(&initial.path)
                .unwrap()
                .contains("old.retained")
        );
    }
}

#[test]
fn process_bootstrap_installs_one_final_layer_even_when_logs_start_disabled() {
    let fixture = Fixture::new();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "observability::logging::tests::bootstrap_worker",
            "--nocapture",
        ])
        .env("FLUXDNS_P1_LOGGING_TEST_ROOT", &fixture.root)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!fixture.root.join("disabled.log").exists());
    let output = fs::read_to_string(fixture.root.join("enabled.log")).unwrap();
    assert!(output.contains("bootstrap.enabled"));
    assert!(!output.contains("bootstrap.disabled"));
    assert_eq!(
        output
            .lines()
            .filter(|line| line.contains("bootstrap.enabled"))
            .count(),
        1
    );
}

#[test]
fn bootstrap_worker() {
    let Some(root) = std::env::var_os("FLUXDNS_P1_LOGGING_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root).canonicalize().unwrap();
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("_fluxdns/p1-logging-tests")
        .canonicalize()
        .unwrap();
    assert_eq!(root.parent(), Some(base.as_path()));
    super::super::init_bootstrap().unwrap();
    super::super::configure_final_output(
        false,
        root.join("disabled.log"),
        super::super::LogLevel::Info,
    )
    .unwrap();
    let writer = super::super::build_runtime_telemetry().unwrap();
    super::super::install_final_tracing(Arc::clone(&writer)).unwrap();
    let off = ResolvedLogs {
        enable: false,
        level: LogLevelDto::Info,
        path: root.join("disabled.log"),
    };
    let owner = LoggingOwner::from_bootstrap(off.clone(), Arc::clone(&writer)).unwrap();
    tracing::error!(event = "bootstrap.disabled", "disabled");
    writer.flush_now(budget()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        owner
            .prepare(
                &off,
                ResolvedLogs {
                    enable: true,
                    level: LogLevelDto::Info,
                    path: root.join("enabled.log"),
                },
                budget(),
            )
            .await
            .unwrap()
            .publish_with(|| Ok::<_, ()>(()))
            .unwrap();
    });
    tracing::info!(event = "bootstrap.enabled", "enabled");
    writer.shutdown(budget()).unwrap();
}
