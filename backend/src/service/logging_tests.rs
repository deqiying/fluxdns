use super::*;
use crate::config::model::LogLevelDto;
use crate::observability::{LoggingError, LoggingOwner};
use crate::ports::telemetry::{EventName, LogEvent, LogLevel, OutcomeClass};
use crate::runtime::SystemSocketFactory;
use hickory_proto::rr::RData;
use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/p1-logging-dns-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns/p1-logging-dns-tests")
            .canonicalize()
            .unwrap();
        let root = self.0.canonicalize().unwrap();
        assert_eq!(root.parent(), Some(base.as_path()));
        fs::remove_dir_all(root).unwrap();
    }
}

fn budget() -> Deadline {
    Deadline::new(Instant::now() + Duration::from_secs(5))
}

fn event(name: &str, level: LogLevel) -> LogEvent {
    LogEvent {
        occurred_at: SystemTime::now(),
        level,
        name: EventName::parse(name.to_owned()).unwrap(),
        component: TelemetryComponent::Application,
        request_digest: None,
        configured_id: None,
        outcome: OutcomeClass::Success,
        runtime_revision: None,
        message: "logging service test",
    }
}

/// 复用实际 service、SQLite/Resolution owner 和 UDP socket，日志关闭不切断指标或 DNS。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logging_switches_and_failed_path_keep_dns_and_process_metrics_running() {
    let fixture = Fixture::new();
    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let path = fixture.0.to_string_lossy().replace('\\', "/");
    let initial_config = tests::runtime_config_with_answer_at(&path, port, "127.0.0.1");
    let initial_logs = initial_config.logs.clone();
    let prepared =
        PreparedRuntime::prepare_with_policy_core(Arc::clone(&initial_config), RuntimeRevision(1))
            .unwrap();
    let factory = SystemSocketFactory::new();
    let bound = crate::runtime::bind_prepared(prepared, &factory, budget(), &Cancellation::new())
        .await
        .unwrap();
    let coordinator = Arc::new(RuntimeCoordinator::new(bound));
    let storage = StorageRuntime::open(&initial_config, budget())
        .await
        .unwrap();
    let (logging, writer, _dispatch) = LoggingOwner::for_test(initial_logs.clone());
    let mut service = DnsService::with_default_timeout_from_coordinator_storage_and_telemetry(
        Arc::clone(&coordinator),
        storage,
        Arc::clone(&writer),
    )
    .unwrap();
    service.attach_logging(Arc::clone(&logging)).unwrap();
    let sampler = service.telemetry_sampler.as_ref().unwrap().clone();
    let source = sampler.resolution.as_ref().unwrap().clone();
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let response = tests::udp_query(address, 1, "example.test.").await;
    assert_eq!(response.metadata.id, 1);
    sampler.sample(&writer).unwrap();
    assert!(
        writer
            .metric_snapshot()
            .iter()
            .any(|metric| metric.name == MetricName::ResolutionEventsAccepted)
    );
    assert!(!initial_logs.path.exists());

    let querying = Arc::new(AtomicBool::new(true));
    let running = Arc::clone(&querying);
    let queries = tokio::spawn(async move {
        let mut count = 0u16;
        while running.load(Ordering::Acquire) {
            let response = tests::udp_query(address, count, "example.test.").await;
            assert_eq!(response.metadata.id, count);
            assert!(response.answers.iter().any(|record| matches!(
                &record.data, RData::A(address) if address.0 == Ipv4Addr::new(127, 0, 0, 1)
                    || address.0 == Ipv4Addr::new(127, 0, 0, 2)
            )));
            count = count.checked_add(1).unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        count
    });
    for (revision, enabled, name, level) in [
        (2, true, "first.log", LogLevelDto::Info),
        (3, true, "first.log", LogLevelDto::Warn),
        (4, true, "second.log", LogLevelDto::Debug),
        (5, false, "never.log", LogLevelDto::Debug),
        (6, true, "second.log", LogLevelDto::Info),
    ] {
        // 等待当前 flush 完成，测试不通过自动重放掩盖 Busy。
        writer.flush(budget()).await.unwrap();
        let mut config = tests::runtime_config_with_answer_at(&path, port, "127.0.0.2");
        let logs = &mut Arc::get_mut(&mut config).unwrap().logs;
        logs.enable = enabled;
        logs.path = fixture.0.join(name);
        logs.level = level;
        let expected_logs = logs.clone();
        let active = service
            .reload_prepared(
                PreparedRuntime::prepare_with_policy_core(config, RuntimeRevision(revision))
                    .unwrap(),
                &factory,
                budget(),
                Cancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(active.revision(), RuntimeRevision(revision));
        assert_eq!(active.listeners().local_addrs().unwrap()[0].port(), port);
        assert!(logging.matches(&expected_logs, &writer));
        assert!(Arc::ptr_eq(&writer, service.telemetry.as_ref().unwrap()));
        assert!(Arc::ptr_eq(
            &sampler,
            service.telemetry_sampler.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            &source,
            &service.resolution_runtime.as_ref().unwrap().metrics()
        ));
        writer
            .emit(event(&format!("switch.{revision}"), LogLevel::Info))
            .unwrap();
        writer.flush(budget()).await.unwrap();
        let response = tests::udp_query(address, revision as u16, "example.test.").await;
        assert!(response.answers.iter().any(|record| matches!(
            &record.data, RData::A(address) if address.0 == Ipv4Addr::new(127, 0, 0, 2)
        )));
    }
    let first = fs::read_to_string(fixture.0.join("first.log")).unwrap();
    let second = fs::read_to_string(fixture.0.join("second.log")).unwrap();
    assert!(first.contains("switch.2") && !first.contains("switch.3"));
    assert!(
        second.contains("switch.4") && second.contains("switch.6") && !second.contains("switch.5")
    );
    assert!(!fixture.0.join("never.log").exists());

    let old = coordinator.load();
    let mut bad = tests::runtime_config_with_answer_at(&path, port, "127.0.0.1");
    let logs = &mut Arc::get_mut(&mut bad).unwrap().logs;
    logs.enable = true;
    logs.path = fixture.0.clone();
    let error = service
        .reload_prepared(
            PreparedRuntime::prepare_with_policy_core(bad, RuntimeRevision(7)).unwrap(),
            &factory,
            budget(),
            Cancellation::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ServiceReloadError::Logging(LoggingError::Prepare(_))
    ));
    assert!(Arc::ptr_eq(&old, &coordinator.load()) && !old.is_draining());
    writer
        .emit(event("failed.path.retained", LogLevel::Info))
        .unwrap();
    writer.flush(budget()).await.unwrap();
    assert!(
        fs::read_to_string(fixture.0.join("second.log"))
            .unwrap()
            .contains("failed.path.retained")
    );

    querying.store(false, Ordering::Release);
    let completed = queries.await.unwrap();
    assert!(completed > 0);
    sampler.sample(&writer).unwrap();
    assert!(source.snapshot().accepted >= u64::from(completed) + 6);
    let report = service
        .shutdown(&SystemClock::new(), budget())
        .await
        .unwrap();
    assert!(!report.deadline_expired && writer.stats().closed());
}
