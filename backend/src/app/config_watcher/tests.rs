use std::fs;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Instant;

use hickory_proto::rr::RData;
use tokio::sync::Mutex;

use super::*;
use crate::app::tests::{reload_source, udp_query};
use crate::config::contract::MAX_CONFIG_BYTES;
use crate::config::disk::DiskStatus;
use crate::config::{ConfigV2Loader, LoadOptions};
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::runtime::{PreparedRuntime, RuntimeCoordinator, SystemSocketFactory, bind_prepared};
use crate::service::DnsService;

struct Fixture {
    root: PathBuf,
    source: PathBuf,
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
        fs::write(&source, b"version: 2\n").unwrap();
        Self { root, source }
    }

    fn watcher(&self) -> ConfigFileWatcher {
        ConfigFileWatcher::new(self.source.clone())
    }

    fn read(&self) -> DiskObservation {
        disk::observe(&self.source)
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

async fn sample(watcher: &mut ConfigFileWatcher, generation: u64) -> Option<Tagged> {
    if watcher.reading.is_none() {
        assert_eq!(watcher.poll(generation).await.unwrap(), None);
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while !watcher.reading.as_ref().unwrap().is_finished() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    watcher.poll(generation).await.unwrap()
}

async fn next(watcher: &mut ConfigFileWatcher) -> DiskObservation {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some((_, observation)) = sample(watcher, 0).await {
                return observation;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn stable_observation_requires_two_equal_reads_and_compares_content_only() {
    let fixture = Fixture::new();
    let mut watcher = fixture.watcher();
    assert_eq!(sample(&mut watcher, 0).await, None);
    let initial = next(&mut watcher).await;
    assert_eq!(initial, fixture.read());

    // 同长度修改必须被识别；只比较正文摘要，不看 mtime 或文件身份。先结束在途读取，
    // 避免写入前开始的读取被当作新观测。
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::write(&fixture.source, b"version: 3\n").unwrap();
    let changed = next(&mut watcher).await;
    assert_ne!(changed, initial);
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::rename(&fixture.source, fixture.root.join("previous.yaml")).unwrap();
    fs::write(&fixture.source, b"version: 3\n").unwrap();
    assert_eq!(next(&mut watcher).await, changed);
    assert!(watcher.finish(Duration::from_secs(3)).await);
}

#[tokio::test]
async fn invalid_missing_and_oversized_files_only_change_observation() {
    let fixture = Fixture::new();
    let mut watcher = fixture.watcher();
    next(&mut watcher).await;
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::write(&fixture.source, b"not: [valid yaml").unwrap();
    assert!(matches!(
        next(&mut watcher).await,
        DiskObservation::Readable { .. }
    ));
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::remove_file(&fixture.source).unwrap();
    assert_eq!(next(&mut watcher).await, DiskObservation::Missing);
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::create_dir(&fixture.source).unwrap();
    assert_eq!(next(&mut watcher).await, DiskObservation::Unreadable);
    assert!(watcher.finish(Duration::from_secs(3)).await);
    fs::remove_dir(&fixture.source).unwrap();
    fs::write(&fixture.source, vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap();
    assert_eq!(next(&mut watcher).await, DiskObservation::Unreadable);
    assert!(watcher.finish(Duration::from_secs(3)).await);
}

#[tokio::test]
async fn generation_change_resets_stability() {
    let fixture = Fixture::new();
    let mut watcher = fixture.watcher();
    assert_eq!(sample(&mut watcher, 1).await, None);
    assert_eq!(sample(&mut watcher, 1).await.unwrap().0, 1);
    // 在途读取以开始时的代次标记，代次前进后仍按旧代次上报，由 store 丢弃。
    assert_eq!(sample(&mut watcher, 2).await.unwrap().0, 1);
    // 旧代次与新代次的观测不能拼成稳定对。
    assert_eq!(sample(&mut watcher, 2).await, None);
    assert_eq!(sample(&mut watcher, 2).await.unwrap().0, 2);
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
        (0, initial)
    }));
    let id = watcher.reading.as_ref().unwrap().id();
    for _ in 0..100 {
        assert_eq!(watcher.poll(0).await.unwrap(), None);
        assert_eq!(watcher.reading.as_ref().unwrap().id(), id);
    }
    drop(release);
    assert!(watcher.finish(Duration::from_secs(3)).await);
}

#[tokio::test]
async fn report_updates_store_disk_status_without_touching_document() {
    let fixture = Fixture::new();
    let (source, _) = crate::config::test_support::portable_example();
    fs::write(&fixture.source, &source).unwrap();
    let store = ConfigStore::open_for_test(&fixture.source);
    let document = store.current();
    let watcher = Mutex::new(fixture.watcher());
    let wait_status = |expected: DiskStatus| {
        let store = &store;
        let watcher = &watcher;
        let digest = document.digest.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    report_config_files(watcher, Some(store)).await;
                    if store.disk_status(&digest) == expected {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
        }
    };
    wait_status(DiskStatus::InSync).await;
    fs::write(&fixture.source, format!("{source}\n# external\n")).unwrap();
    wait_status(DiskStatus::Modified).await;
    fs::remove_file(&fixture.source).unwrap();
    wait_status(DiskStatus::Missing).await;
    fs::write(&fixture.source, &source).unwrap();
    wait_status(DiskStatus::InSync).await;
    // 外改只更新观测，内存文档保持原样（I5）。
    assert!(Arc::ptr_eq(&store.current(), &document));
    assert!(watcher.lock().await.finish(Duration::from_secs(3)).await);
}

async fn wait_reported(watcher: &Mutex<ConfigFileWatcher>, expected: &DiskObservation) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if watcher.lock().await.reported.as_ref() == Some(expected) {
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
    let initial = ConfigV2Loader::new(LoadOptions::default().without_snapshot())
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
                        report_config_files(&polling, None).await;
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
        wait_reported(&watcher, &fixture.read()).await;
        for id in 1..=5 {
            match id {
                1 => fs::write(
                    &fixture.source,
                    original
                        .replace(&file_hosts, constant_hosts)
                        .replace("127.0.0.1 example.test", "127.0.0.2 example.test"),
                )
                .unwrap(),
                2 => fs::write(&fixture.source, b"invalid: [yaml").unwrap(),
                3 => fs::remove_file(&fixture.source).unwrap(),
                4 => fs::write(&fixture.source, vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap(),
                5 => fs::write(&fixture.source, &original).unwrap(),
                _ => unreachable!(),
            }
            wait_reported(&watcher, &fixture.read()).await;
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
