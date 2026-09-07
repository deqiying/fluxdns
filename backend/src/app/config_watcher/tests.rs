use std::fs;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Instant;

use hickory_proto::rr::RData;
use tokio::sync::Mutex;

use super::*;
use crate::app::tests::{reload_source, udp_query};
use crate::config::contract::MAX_CONFIG_BYTES;
use crate::config::store::observation::FileObservation;
use crate::config::{ConfigLoader, LoadOptions};
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::runtime::{PreparedRuntime, RuntimeCoordinator, SystemSocketFactory, bind_prepared};
use crate::service::DnsService;

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    derived: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p1-file-watch");
        fs::create_dir_all(&base).unwrap();
        let root = fs::canonicalize(base).unwrap().join(suffix);
        fs::create_dir(&root).unwrap();
        let source = root.join("source.yaml");
        let derived = root.join("config.yaml");
        fs::write(&source, b"version: 2\n").unwrap();
        fs::write(&derived, b"version: 2\n").unwrap();
        Self {
            root,
            source,
            derived,
        }
    }

    fn watcher(&self) -> ConfigFileWatcher {
        ConfigFileWatcher::new(self.source.clone(), Some(self.derived.clone()))
    }

    fn read(&self) -> ManagedObservation {
        ManagedObservation::read(&self.source, Some(&self.derived))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base = fs::canonicalize(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p1-file-watch"),
        )
        .unwrap();
        assert_eq!(self.root.parent(), Some(base.as_path()));
        assert_eq!(fs::canonicalize(&self.root).unwrap(), self.root);
        fs::remove_dir_all(&self.root).unwrap();
    }
}

async fn sample(watcher: &mut ConfigFileWatcher) -> Option<ManagedObservation> {
    if watcher.reading.is_none() {
        assert_eq!(watcher.poll_change().await.unwrap(), None);
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while !watcher.reading.as_ref().unwrap().is_finished() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    watcher.poll_change().await.unwrap()
}

async fn next(watcher: &mut ConfigFileWatcher) -> ManagedObservation {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(observation) = sample(watcher).await {
                return observation;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn first_observation_is_neutral_and_debounces_both_files() {
    let fixture = Fixture::new();
    let mut watcher = fixture.watcher();
    assert_eq!(sample(&mut watcher).await, None);
    let initial = next(&mut watcher).await;
    assert_eq!(initial, fixture.read());
    assert_eq!(sample(&mut watcher).await, None);

    fs::write(&fixture.derived, b"version: 3\n").unwrap();
    let derived_changed = next(&mut watcher).await;
    assert_eq!(derived_changed.source, initial.source);
    assert_ne!(derived_changed.revision(), initial.revision());
    assert_eq!(sample(&mut watcher).await, None);

    // 同长度修改和同内容换文件均进入组合观测，不能只检查 mtime/长度。
    fs::write(&fixture.source, b"version: 3\n").unwrap();
    let changed = next(&mut watcher).await;
    assert_ne!(changed.source, initial.source);
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::rename(&fixture.source, fixture.root.join("previous.yaml")).unwrap();
    fs::write(&fixture.source, b"version: 3\n").unwrap();
    let replaced = next(&mut watcher).await;
    assert_ne!(replaced.source, changed.source);
    assert!(watcher.finish(Duration::from_secs(3)).await);
}

#[tokio::test]
async fn invalid_missing_and_oversized_files_only_change_observation() {
    let fixture = Fixture::new();
    let mut watcher = fixture.watcher();
    next(&mut watcher).await;
    fs::write(&fixture.source, b"not: [valid yaml").unwrap();
    assert!(matches!(
        next(&mut watcher).await.source,
        FileObservation::Readable { .. }
    ));
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::remove_file(&fixture.source).unwrap();
    assert_eq!(next(&mut watcher).await.source, FileObservation::Missing);
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::create_dir(&fixture.source).unwrap();
    assert_eq!(next(&mut watcher).await.source, FileObservation::Unreadable);
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::remove_dir(&fixture.source).unwrap();
    fs::write(&fixture.source, vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap();
    assert_eq!(next(&mut watcher).await.source, FileObservation::Oversized);
    assert!(watcher.finish(Duration::from_secs(3)).await);
}

#[tokio::test]
async fn slow_reader_never_blocks_poll_or_queues_another_read() {
    let fixture = Fixture::new();
    let mut watcher = fixture.watcher();
    let initial = fixture.read();
    let (release, waiting) = std::sync::mpsc::channel::<()>();
    watcher.reading = Some(tokio::task::spawn_blocking(move || {
        let _ = waiting.recv();
        initial
    }));
    let id = watcher.reading.as_ref().unwrap().id();
    for _ in 0..100 {
        assert_eq!(watcher.poll_change().await.unwrap(), None);
        assert_eq!(watcher.reading.as_ref().unwrap().id(), id);
    }
    drop(release);
    assert!(watcher.finish(Duration::from_secs(3)).await);
}

async fn wait_observed(watcher: &Mutex<ConfigFileWatcher>, expected: &ManagedObservation) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if watcher.lock().await.observed.as_ref() == Some(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn production_service_loop_keeps_dns_and_revision_during_external_changes() {
    let fixture = Fixture::new();
    let reserved = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    let resource_path = fixture.root.join("hosts.txt");
    fs::write(&resource_path, "127.0.0.1 example.test\n").unwrap();
    let constant_hosts = "  - type: const\n    name: local-hosts\n    format: hosts\n    hosts: \"127.0.0.1 example.test\"";
    let file_hosts = format!(
        "  - type: file\n    name: local-hosts\n    format: hosts\n    path: {}\n    auto_update: true\n    update_interval: 1s",
        resource_path.display()
    );
    let original = reload_source(&fixture.root, port).replace(constant_hosts, &file_hosts);
    fs::write(&fixture.source, &original).unwrap();
    fs::write(&fixture.derived, &original).unwrap();
    let initial = ConfigLoader::new(LoadOptions::default().without_snapshot())
        .load_from_path(&fixture.source)
        .unwrap()
        .resolved;
    let prepared = PreparedRuntime::prepare_with_policy_core_and_remote_resources(
        initial,
        RuntimeRevision(1),
        Deadline::new(Instant::now() + Duration::from_secs(5)),
        Cancellation::new(),
    )
    .await
    .unwrap();
    drop(reserved);
    let candidate = bind_prepared(
        prepared,
        &SystemSocketFactory::new(),
        Deadline::new(Instant::now() + Duration::from_secs(5)),
        &Cancellation::new(),
    )
    .await
    .unwrap();
    let coordinator = Arc::new(RuntimeCoordinator::new(candidate));
    let mut service =
        DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator)).unwrap();
    let initial_runtime = coordinator.load();
    let address = initial_runtime.listeners().local_addrs().unwrap()[0];
    let watcher = Arc::new(Mutex::new(fixture.watcher()));
    let polling = Arc::clone(&watcher);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = async {
        service
            .run_with_reload(
                Duration::from_secs(3),
                Duration::from_millis(10),
                move |_| {
                    let polling = Arc::clone(&polling);
                    Box::pin(async move {
                        report_config_files(&polling).await;
                        Ok(())
                    })
                },
                async move {
                    stopped.await.unwrap();
                    Ok(())
                },
            )
            .await
    };
    let changes = async {
        wait_observed(&watcher, &fixture.read()).await;
        for id in 1..=6 {
            match id {
                1 => fs::write(
                    &fixture.source,
                    original
                        .replace(&file_hosts, constant_hosts)
                        .replace("127.0.0.1 example.test", "127.0.0.2 example.test"),
                )
                .unwrap(),
                2 => fs::write(&fixture.derived, b"invalid: [yaml").unwrap(),
                3 => fs::write(&fixture.source, b"invalid: [yaml").unwrap(),
                4 => fs::remove_file(&fixture.source).unwrap(),
                5 => fs::write(&fixture.source, vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap(),
                6 => fs::write(&fixture.source, &original).unwrap(),
                _ => unreachable!(),
            }
            wait_observed(&watcher, &fixture.read()).await;
            let response = udp_query(address, id, "example.test.").await;
            assert!(response.answers.iter().any(|record| matches!(
                &record.data, RData::A(ip) if ip.0 == Ipv4Addr::new(127, 0, 0, 1)
            )));
            assert!(Arc::ptr_eq(&initial_runtime, &coordinator.load()));
            assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
            assert!(!initial_runtime.is_draining());
        }
        // 仅改资源文件，由正式 resource worker 到期刷新，不手动调用 refresh_resource。
        fs::write(&resource_path, "127.0.0.3 example.test\n").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response = udp_query(address, 7, "example.test.").await;
                if response.answers.iter().any(|record| {
                    matches!(
                        &record.data, RData::A(ip) if ip.0 == Ipv4Addr::new(127, 0, 0, 3)
                    )
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(Arc::ptr_eq(&initial_runtime, &coordinator.load()));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
        stop.send(()).unwrap();
    };
    let (report, ()) = tokio::join!(running, changes);
    assert!(!report.unwrap().deadline_expired);
    assert!(watcher.lock().await.finish(Duration::from_secs(3)).await);
}
