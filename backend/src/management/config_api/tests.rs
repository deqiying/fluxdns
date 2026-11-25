//! 配置写接口的 HTTP 契约：真实 DnsService 控制循环 + 真实 ServiceHost，验证保存、外改冲突、
//! 覆盖、重新加载、写入磁盘与失败信封。

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::config::store::ConfigStore;
use crate::config::{ConfigV2Loader, LoadOptions};
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::management::auth::AuthState;
use crate::management::router::{AuthServices, build_router};
use crate::management::session::SessionStore;
use crate::runtime::{PreparedRuntime, RuntimeCoordinator, SystemSocketFactory, bind_prepared};
use crate::service::DnsService;

const ORIGIN_VALUE: &str = "http://127.0.0.1:8080";

fn config_source(work: &str, port: u16, extra_work: &str) -> String {
    format!(
        r#"# 配置接口测试：此注释必须在条目级写回后保留
version: 2
work:
  path: {work}
  rules_path: ./rules{extra_work}
database:
  type: sqlite
  path: ./data.sqlite
  records_path: ./queries
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
    port: {port}
    strategy: default
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "127.0.0.1 example.test"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: "127.0.0.1 example.test"
outbound: []
rule_set: []
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
clients: []
"#
    )
}

struct Harness {
    root: PathBuf,
    source: PathBuf,
    app: Router,
    store: Arc<ConfigStore>,
    coordinator: Arc<RuntimeCoordinator>,
    authorization: String,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Harness {
    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(AUTHORIZATION, &self.authorization)
            .header(ORIGIN, ORIGIN_VALUE)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn file(&self) -> String {
        fs::read_to_string(&self.source).unwrap()
    }

    fn revision(&self) -> u64 {
        self.coordinator.current_revision().0
    }
}

/// 启动绑定真实 UDP 端口的 Runtime 与服务控制循环所需的全部对象；`extra_work` 追加到
/// `work` 段末尾。
async fn start(extra_work: &str) -> (Harness, DnsService) {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let name = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p2-config-api");
    fs::create_dir_all(&base).unwrap();
    let root = fs::canonicalize(base).unwrap().join(name);
    fs::create_dir_all(&root).unwrap();
    let reserved = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    // 生产启动时工作目录已由快照创建；镜像写入不负责建目录。
    fs::create_dir_all(root.join("work")).unwrap();
    let work = root.join("work").to_string_lossy().replace('\\', "/");
    let source = root.join("source.yaml");
    fs::write(&source, config_source(&work, port, extra_work)).unwrap();
    let initial = ConfigV2Loader::new(LoadOptions::default().without_snapshot())
        .load_from_path(&source)
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
    let service =
        DnsService::with_default_timeout_from_coordinator(Arc::clone(&coordinator)).unwrap();
    let store = Arc::new(ConfigStore::open_for_test(&source));
    let sessions = Arc::new(SessionStore::new(false));
    let issued = sessions.issue("admin".to_owned()).unwrap();
    let services = AuthServices::new(
        Arc::new(AuthState::new(&[]).unwrap()),
        sessions,
        Arc::clone(&store),
        ORIGIN_VALUE.to_owned(),
        None,
    )
    .with_config_control(service.control(), Arc::clone(&coordinator));
    (
        Harness {
            root,
            source,
            app: build_router(Arc::new(services)),
            store,
            coordinator,
            authorization: format!("Bearer {}", issued.view.access_token),
        },
        service,
    )
}

/// 在服务控制循环运行期间执行测试体，结束后正常关闭服务。
async fn run<F: std::future::Future<Output = ()>>(mut service: DnsService, body: F) {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = service.run_with_reload(
        Duration::from_secs(3),
        Duration::from_millis(50),
        |_| Box::pin(async { Ok(()) }),
        async move {
            let _ = stopped.await;
            Ok(())
        },
    );
    let test = async move {
        body.await;
        stop.send(()).unwrap();
    };
    let (report, ()) = tokio::join!(running, test);
    assert!(!report.unwrap().deadline_expired);
}

/// 修改本地 hosts 资源内容：普通 Runtime 变更（测试进程内不能切换全局日志配置）。
fn hosts(address: &str) -> Value {
    json!({"changes": [{"module": "hosts", "change": {
        "action": "update", "original_name": "local-hosts",
        "value": {"type": "const", "name": "local-hosts", "format": "hosts",
                  "hosts": format!("{address} example.test")}
    }}]})
}

#[tokio::test]
async fn save_reload_and_persist_follow_disk_status_contract() {
    let (harness, service) = start("").await;
    let harness = &harness;
    run(service, async move {
        // 保存：应用 Runtime、写源文件与镜像，条目级写回保留注释。
        let (status, body) = harness
            .post("/api/v2/config/save", hosts("127.0.0.2"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["persisted"], true);
        assert!(body.get("diagnostic").is_none(), "{body}");
        assert_eq!(body["state"]["runtime_revision"], "2");
        assert_eq!(body["state"]["disk"]["status"], "in_sync");
        assert_eq!(
            body["state"]["config_digest"],
            harness.store.current().digest
        );
        assert_eq!(harness.revision(), 2);
        let saved = harness.file();
        assert!(saved.contains("127.0.0.2 example.test"));
        assert!(saved.starts_with("# 配置接口测试"));
        assert_eq!(
            fs::read_to_string(harness.root.join("work/config.yaml")).unwrap(),
            saved
        );

        // 外部修改后保存被拒绝，只带磁盘状态，Runtime 不变。
        fs::write(&harness.source, format!("{saved}# external\n")).unwrap();
        let (status, body) = harness
            .post("/api/v2/config/save", hosts("127.0.0.3"))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "CONFIG_FILE_CHANGED");
        assert_eq!(body["disk"]["status"], "modified");
        assert_eq!(harness.revision(), 2);

        // 选择覆盖：以内存配置加本次修改写盘。
        let mut overwrite = hosts("127.0.0.3");
        overwrite["overwrite_disk"] = json!(true);
        let (status, body) = harness.post("/api/v2/config/save", overwrite).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["persisted"], true);
        assert_eq!(body["state"]["runtime_revision"], "3");
        assert!(!harness.file().contains("# external"));

        // 外部合法修改：重新加载并应用。
        let external = harness
            .file()
            .replace("127.0.0.3 example.test", "127.0.0.4 example.test");
        assert_ne!(external, harness.file());
        fs::write(&harness.source, &external).unwrap();
        let (status, body) = harness.post("/api/v2/config/reload", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"]["runtime_revision"], "4");
        assert_eq!(body["state"]["disk"]["status"], "in_sync");
        assert_eq!(&*harness.store.current().text, external);
        assert_eq!(harness.revision(), 4);

        // 文件被删除：重新加载失败，写入磁盘恢复文件且不切换 Runtime。
        fs::remove_file(&harness.source).unwrap();
        let (status, body) = harness.post("/api/v2/config/reload", json!({})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "CONFIG_FILE_UNREADABLE");
        assert_eq!(body["diagnostic"]["code"], "CONFIG_FILE_MISSING");
        let (status, body) = harness.post("/api/v2/config/persist", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"]["disk"]["status"], "in_sync");
        assert_eq!(body["state"]["runtime_revision"], "4");
        assert_eq!(harness.file(), external);
        assert_eq!(harness.revision(), 4);
    })
    .await;
}

#[tokio::test]
async fn rejected_saves_keep_runtime_file_and_document() {
    let (harness, service) = start("").await;
    let harness = &harness;
    run(service, async move {
        let original = harness.file();
        let document = harness.store.current();

        let (status, body) = harness
            .post("/api/v2/config/save", json!({"changes": []}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "INVALID_ARGUMENT");

        // 引用不存在的上游：候选校验失败，返回字段路径。
        let (status, body) = harness
            .post(
                "/api/v2/config/save",
                json!({"changes": [{"module": "strategy", "change": {
                    "action": "update", "original_name": "default",
                    "value": {"name": "default", "rules": [], "default_upstream": "missing"}
                }}]}),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "VALIDATION_FAILED");
        assert!(!body["field_errors"].as_array().unwrap().is_empty());

        // 缩短统计保留期需要显式确认。
        let shorten = json!({"changes": [{"module": "statistics", "change": {
            "retention": {"days": 1, "grace_days": 0, "reference_size_bytes": 1}
        }}]});
        let (status, body) = harness.post("/api/v2/config/save", shorten).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "CONFIRMATION_REQUIRED");
        assert_eq!(
            body["diagnostic"]["code"],
            "RETENTION_SHORTENING_UNCONFIRMED"
        );

        assert_eq!(harness.revision(), 1);
        assert_eq!(harness.file(), original);
        assert!(Arc::ptr_eq(&document, &harness.store.current()));
    })
    .await;
}

#[tokio::test]
async fn process_owned_change_is_saved_with_restart_hint() {
    let (harness, service) = start("").await;
    let harness = &harness;
    run(service, async move {
        let (status, body) = harness
            .post(
                "/api/v2/config/save",
                json!({"changes": [{"module": "dns", "change": {"resolve_log": {"enable": true}}}]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["persisted"], true);
        assert_eq!(harness.revision(), 2);
        assert_eq!(
            harness.store.current().restart_required,
            vec!["dns.resolve_log"]
        );
        // Runtime 沿用启动时的值，重启后才生效。
        assert!(
            !harness
                .coordinator
                .load()
                .snapshot()
                .config()
                .dns
                .resolve_log
                .enable
        );
        assert!(harness.file().contains("resolve_log"));
    })
    .await;
}

#[tokio::test]
async fn remote_dat_limit_failure_returns_safe_apply_diagnostic() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (harness, service) = start("\n  rule_set_max_rules: 1").await;
    let harness = &harness;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // 两条 full 域名的 GeoSite protobuf；限制 1，且 strategy 引用 `cn`，确保该 selector
    // 经过真正下载和 DAT 解析后因实际加载的规则数超限而拒绝。
    let mut site = vec![0x0a, 2, b'C', b'N'];
    for name in ["a.test", "b.test"] {
        let mut domain = vec![0x08, 3, 0x12, name.len() as u8];
        domain.extend_from_slice(name.as_bytes());
        site.extend_from_slice(&[0x12, domain.len() as u8]);
        site.extend(domain);
    }
    let mut payload = vec![0x0a, site.len() as u8];
    payload.extend(site);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let length = stream.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..length]).contains("private-token-sentinel"));
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(&payload).await.unwrap();
    });
    run(service, async move {
        let original = harness.file();
        let request = json!({"changes": [
            {"module": "rule_set", "change": {"action": "create", "value": {
                "name": "diagnostic-dat", "type": "remote", "format": "dat",
                "url": format!("http://{address}/geosite.dat?token=private-token-sentinel"),
                "auto_update": false
            }}},
            {"module": "strategy", "change": {"action": "create", "value": {
                "name": "diagnostic-dat-strategy",
                "rules": [{"rule_set": "diagnostic-dat:cn", "upstream": "local"}],
                "default_upstream": "local"
            }}}
        ]});
        let (status, body) = harness.post("/api/v2/config/save", request).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "APPLY_FAILED");
        assert_eq!(body["diagnostic"]["code"], "RULE_TOO_MANY_RULES");
        assert_eq!(body["diagnostic"]["stage"], "resource_parse");
        assert_eq!(body["diagnostic"]["resource"], "diagnostic-dat");
        let encoded = body.to_string();
        assert!(!encoded.contains("private-token-sentinel"));
        assert!(!encoded.contains(&address.to_string()));
        assert_eq!(harness.revision(), 1);
        assert_eq!(harness.file(), original);
    })
    .await;
    server.await.unwrap();
}

#[tokio::test]
async fn old_config_routes_are_gone_and_writes_require_bearer_and_origin() {
    let (harness, service) = start("").await;
    let harness = &harness;
    run(service, async move {
        for (method, path) in [
            ("POST", "/api/v2/config/validate"),
            ("POST", "/api/v2/config/apply"),
            ("POST", "/api/v2/config/modules/logs/validate"),
            ("POST", "/api/v2/config/modules/logs/apply"),
            ("GET", "/api/v2/config/operations/operation-1"),
            ("GET", "/api/v2/config/files/diff"),
            ("POST", "/api/v2/config/files/restore"),
            ("POST", "/api/v2/config/files/retry"),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header(AUTHORIZATION, &harness.authorization)
                .header(ORIGIN, ORIGIN_VALUE)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let response = harness.app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }
        for path in [
            "/api/v2/config/save",
            "/api/v2/config/reload",
            "/api/v2/config/persist",
        ] {
            let unauthenticated = Request::builder()
                .method("POST")
                .uri(path)
                .header(ORIGIN, ORIGIN_VALUE)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let response = harness.app.clone().oneshot(unauthenticated).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            let foreign = Request::builder()
                .method("POST")
                .uri(path)
                .header(AUTHORIZATION, &harness.authorization)
                .header(ORIGIN, "http://example.invalid")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let response = harness.app.clone().oneshot(foreign).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
        assert_eq!(harness.revision(), 1);
    })
    .await;
}
