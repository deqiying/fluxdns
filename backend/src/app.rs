use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::model::LogLevelDto;
use crate::config::resolve::SecretValidationError;
use crate::config::{ConfigLoadError, ConfigLoader, LoadOptions};
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::observability;
use crate::ports::effects::SocketFactory;
use crate::runtime::{
    ActiveRuntime, PrepareError, PreparedRuntime, RuntimeCoordinator, SystemSocketFactory,
};
use crate::service::{
    DnsService, ServiceError, ServiceReloadError, ServiceStartError, process_owned_reload_change,
};

mod config_watcher;

use config_watcher::{ConfigFileWatcher, report_config_files};

/// 进程退出码的大类，详细原因应由安全错误消息表达。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AppExitCode {
    Success = 0,
    InvalidInput = 2,
    PrepareFailure = 3,
    StartupFailure = 4,
    RuntimeFailure = 5,
}

impl AppExitCode {
    pub const fn value(self) -> u8 {
        self as u8
    }
}

/// Application 边界稳定的错误分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppErrorKind {
    CliOrConfig,
    Prepare,
    BindOrStartup,
    RuntimeFatal,
    ShutdownTimeout,
}

impl AppErrorKind {
    pub const fn exit_code(self) -> AppExitCode {
        match self {
            Self::CliOrConfig => AppExitCode::InvalidInput,
            Self::Prepare => AppExitCode::PrepareFailure,
            Self::BindOrStartup => AppExitCode::StartupFailure,
            Self::RuntimeFatal | Self::ShutdownTimeout => AppExitCode::RuntimeFailure,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CliOrConfig => "cli_or_config",
            Self::Prepare => "prepare",
            Self::BindOrStartup => "bind_or_startup",
            Self::RuntimeFatal => "runtime_fatal",
            Self::ShutdownTimeout => "shutdown_timeout",
        }
    }
}

impl fmt::Display for AppErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 可跨进程边界展示的 Application 错误。
///
/// `safe_message` 只接收已限制长度的安全摘要，禁止把原始 YAML、DNS wire 或 secret
/// 值带到 stderr。
#[derive(Debug, thiserror::Error)]
#[error("{safe_message}")]
pub struct AppError {
    kind: AppErrorKind,
    safe_message: String,
}

impl AppError {
    pub fn new(kind: AppErrorKind, safe_message: impl Into<String>) -> Self {
        Self {
            kind,
            safe_message: safe_message.into(),
        }
    }

    pub const fn kind(&self) -> AppErrorKind {
        self.kind
    }

    pub fn safe_message(&self) -> &str {
        &self.safe_message
    }

    pub const fn exit_code(&self) -> AppExitCode {
        self.kind.exit_code()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppCommand {
    Run,
    Validate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliOptions {
    pub command: AppCommand,
    pub config_path: PathBuf,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            command: AppCommand::Run,
            config_path: PathBuf::from("config.yaml"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliAction {
    Command,
    Help,
    Version,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CliError {
    #[error("unknown command or argument")]
    UnknownArgument,
    #[error("an argument value is missing")]
    MissingValue,
    #[error("only one command may be specified")]
    MultipleCommands,
    #[error("a configuration path may only be specified once")]
    DuplicateConfigPath,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplicationReloadError {
    #[error("runtime reload configuration load failed: {0}")]
    Config(#[source] ConfigLoadError),
    #[error("runtime reload secret validation failed: {0}")]
    SecretValidation(#[source] SecretValidationError),
    #[error("runtime reload revision space is exhausted")]
    RevisionExhausted,
    #[error("runtime reload prepare failed: {0}")]
    Prepare(#[source] PrepareError),
    #[error(
        "runtime reload changes process-owned {component} configuration and requires process restart"
    )]
    RestartRequired { component: &'static str },
    #[error("runtime reload activation failed: {0}")]
    Activation(#[source] crate::runtime::RuntimeReloadError),
    #[error("runtime reload service activation failed: {0}")]
    Service(#[source] ServiceReloadError),
}

/// 解析进程边界参数；不访问文件系统，也不猜测配置位置。
pub fn parse_args<I>(args: I) -> Result<(CliAction, CliOptions), CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut options = CliOptions::default();
    let mut command_seen = false;
    let mut config_seen = false;
    let mut iter = args.into_iter();

    while let Some(argument) = iter.next() {
        if argument == "--help" || argument == "-h" {
            return Ok((CliAction::Help, options));
        }
        if argument == "--version" || argument == "-V" {
            return Ok((CliAction::Version, options));
        }

        if argument == "run" || argument == "validate" {
            if command_seen {
                return Err(CliError::MultipleCommands);
            }
            command_seen = true;
            options.command = if argument == "validate" {
                AppCommand::Validate
            } else {
                AppCommand::Run
            };
            continue;
        }

        if argument == "--config" || argument == "-c" {
            if config_seen {
                return Err(CliError::DuplicateConfigPath);
            }
            options.config_path = iter.next().ok_or(CliError::MissingValue)?.into();
            if options.config_path.as_os_str().is_empty() {
                return Err(CliError::MissingValue);
            }
            config_seen = true;
            continue;
        }

        if let Some(value) = argument
            .to_str()
            .and_then(|value| value.strip_prefix("--config="))
        {
            if config_seen || value.is_empty() {
                return if config_seen {
                    Err(CliError::DuplicateConfigPath)
                } else {
                    Err(CliError::MissingValue)
                };
            }
            options.config_path = PathBuf::from(value);
            config_seen = true;
            continue;
        }

        return Err(CliError::UnknownArgument);
    }

    Ok((CliAction::Command, options))
}

const BIND_TIMEOUT: Duration = Duration::from_secs(10);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(30);
const CONFIG_OBSERVATION_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// 根据当前命令加载并执行配置边界。
pub async fn run() -> Result<(), AppError> {
    run_with_args(std::env::args_os().skip(1)).await
}

pub async fn run_with_args<I>(args: I) -> Result<(), AppError>
where
    I: IntoIterator<Item = OsString>,
{
    let (action, options) = parse_args(args).map_err(|error| {
        AppError::new(AppErrorKind::CliOrConfig, format!("CLI 参数无效：{error}"))
    })?;

    match action {
        CliAction::Help => {
            print_help();
            Ok(())
        }
        CliAction::Version => {
            println!("fluxdns {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CliAction::Command => run_command(options).await,
    }
}

/// 从配置文件准备并激活下一版运行时。
///
/// reload 只读取并校验输入，不创建工作目录配置 snapshot；候选在 bind 和
/// revision CAS 成功前不会改变当前 ActiveRuntime。
pub async fn reload_runtime_from_path(
    coordinator: &RuntimeCoordinator,
    path: impl AsRef<std::path::Path>,
    factory: &dyn SocketFactory,
    deadline: Deadline,
    cancellation: Cancellation,
) -> Result<Arc<ActiveRuntime>, ApplicationReloadError> {
    let current = coordinator.load();
    let expected = current.revision();
    let prepared = prepare_reload_candidate(
        current.snapshot().config(),
        expected,
        path,
        deadline,
        cancellation.clone(),
        false,
    )
    .await?;

    coordinator
        .bind_and_activate(expected, prepared, factory, deadline, &cancellation)
        .await
        .map_err(ApplicationReloadError::Activation)
}

/// 从配置文件准备并通过已有 service 重建 listener task。
pub async fn reload_service_from_path(
    service: &mut DnsService,
    path: impl AsRef<std::path::Path>,
    factory: &dyn SocketFactory,
    deadline: Deadline,
    cancellation: Cancellation,
) -> Result<Arc<ActiveRuntime>, ApplicationReloadError> {
    let current = Arc::clone(service.runtime());
    let expected = current.revision();
    let prepared = prepare_reload_candidate(
        current.snapshot().config(),
        expected,
        path,
        deadline,
        cancellation.clone(),
        true,
    )
    .await?;
    service
        .reload_prepared(prepared, factory, deadline, cancellation)
        .await
        .map_err(ApplicationReloadError::Service)
}

async fn prepare_reload_candidate(
    current: &crate::config::ResolvedConfig,
    expected: RuntimeRevision,
    path: impl AsRef<std::path::Path>,
    deadline: Deadline,
    cancellation: Cancellation,
    allow_logging_change: bool,
) -> Result<PreparedRuntime, ApplicationReloadError> {
    let output = ConfigLoader::new(LoadOptions::default().without_snapshot())
        .load_from_path(path)
        .map_err(ApplicationReloadError::Config)?;
    output
        .resolved
        .validate_secret_refs(64 * 1024)
        .map_err(ApplicationReloadError::SecretValidation)?;
    if let Some(component) = process_owned_reload_change(current, &output.resolved) {
        return Err(ApplicationReloadError::RestartRequired { component });
    }
    // 仅 coordinator 的旧入口没有日志 owner，不能发布“字段已变、输出未变”的假成功。
    if !allow_logging_change && current.logs != output.resolved.logs {
        return Err(ApplicationReloadError::Service(
            ServiceReloadError::Logging(observability::LoggingError::Unavailable),
        ));
    }
    let revision = RuntimeRevision(
        expected
            .0
            .checked_add(1)
            .ok_or(ApplicationReloadError::RevisionExhausted)?,
    );
    PreparedRuntime::prepare_with_policy_core_and_remote_resources(
        output.resolved,
        revision,
        deadline,
        cancellation,
    )
    .await
    .map_err(ApplicationReloadError::Prepare)
}

async fn run_command(options: CliOptions) -> Result<(), AppError> {
    if options.command == AppCommand::Run {
        crate::config::store::recover_pending_transaction(&options.config_path)
            .map_err(|error| AppError::new(AppErrorKind::Prepare, bounded_message(error)))?;
    }
    let load_options = match options.command {
        AppCommand::Run => LoadOptions::default(),
        AppCommand::Validate => LoadOptions::default().without_snapshot(),
    };
    let output = ConfigLoader::new(load_options)
        .load_from_path(&options.config_path)
        .map_err(map_config_error)?;
    if options.command == AppCommand::Run {
        output
            .resolved
            .validate_secret_refs(64 * 1024)
            .map_err(|error| AppError::new(AppErrorKind::Prepare, bounded_message(error)))?;
        observability::configure_final_output(
            output.resolved.logs.enable,
            &output.resolved.logs.path,
            match output.resolved.logs.level {
                LogLevelDto::Trace => observability::LogLevel::Trace,
                LogLevelDto::Debug => observability::LogLevel::Debug,
                LogLevelDto::Info => observability::LogLevel::Info,
                LogLevelDto::Warn => observability::LogLevel::Warn,
                LogLevelDto::Error => observability::LogLevel::Error,
            },
        )
        .map_err(|_| AppError::new(AppErrorKind::Prepare, "日志输出初始化失败"))?;
    }

    match options.command {
        AppCommand::Validate => {
            tracing::info!(
                event = "configuration_validated",
                component = "application",
                result = "success",
                listener_count = output.resolved.listeners.len(),
                upstream_count = output.resolved.upstreams.len(),
                strategy_count = output.resolved.strategies.len(),
                "configuration_validated"
            );
            Ok(())
        }
        AppCommand::Run => {
            let watched_paths = output.source_path.clone().map(|source| {
                let derived = (source != output.resolved.work.snapshot_path)
                    .then(|| output.resolved.work.snapshot_path.clone());
                (source, derived)
            });
            let management_bootstrap = output.resolved.webui.enable.then(|| {
                (
                    output.resolved.webui.clone(),
                    output.source_path.clone(),
                    output.resolved.work.snapshot_path.clone(),
                    output.resolved.input_hash.clone(),
                    output.resolved.database.path.clone(),
                    output.resolved.dns.resolve_log.enable,
                )
            });
            // 日志关闭只关闭输出，指标、health 和进程 writer 的生命周期不随之退出。
            let telemetry = observability::build_runtime_telemetry()
                .map_err(|error| AppError::new(AppErrorKind::Prepare, bounded_message(error)))?;
            observability::install_final_tracing(Arc::clone(&telemetry))
                .map_err(|error| AppError::new(AppErrorKind::Prepare, bounded_message(error)))?;
            let logging = observability::LoggingOwner::from_bootstrap(
                output.resolved.logs.clone(),
                Arc::clone(&telemetry),
            )
            .map_err(|error| AppError::new(AppErrorKind::Prepare, bounded_message(error)))?;
            let prepared =
                crate::runtime::PreparedRuntime::prepare_with_policy_core_and_remote_resources(
                    output.resolved,
                    RuntimeRevision(1),
                    Deadline::new(Instant::now() + PREPARE_TIMEOUT),
                    Cancellation::new(),
                )
                .await
                .map_err(|error| AppError::new(AppErrorKind::Prepare, bounded_message(error)))?;
            tracing::info!(
                event = "runtime_prepared",
                component = "application",
                result = "success",
                revision = prepared.preflight().revision.0,
                endpoint_count = prepared.preflight().endpoint_count,
                policy_core = prepared.preflight().has_policy_core,
                resource_snapshot_count = prepared.preflight().resource_snapshot_count,
                resource_worker_count = prepared.preflight().resource_worker_count,
                "runtime_prepared"
            );
            let bind_cancellation = crate::dns::Cancellation::new();
            let socket_factory = SystemSocketFactory::new();
            let storage = crate::storage::StorageRuntime::open(
                prepared.snapshot().config(),
                Deadline::new(Instant::now() + PREPARE_TIMEOUT),
            )
            .await
            .map_err(map_storage_prepare_error)?;
            let resolution_metrics = storage.resolution_metrics();
            let candidate = crate::runtime::bind_prepared(
                prepared,
                &socket_factory,
                Deadline::new(Instant::now() + BIND_TIMEOUT),
                &bind_cancellation,
            )
            .await
            .map_err(map_bind_error)?;
            let coordinator = Arc::new(crate::runtime::RuntimeCoordinator::new(candidate));
            let management = match management_bootstrap {
                Some((
                    config,
                    Some(source_path),
                    snapshot_path,
                    source_fingerprint,
                    database_path,
                    resolve_log_enabled,
                )) => Some(
                    crate::management::ManagementService::bind(
                        &config,
                        source_path,
                        snapshot_path,
                        source_fingerprint,
                        crate::management::ManagementQueryDependencies::new(
                            Arc::clone(&coordinator),
                            database_path,
                            resolve_log_enabled,
                            Some(Arc::clone(&telemetry)),
                            resolution_metrics,
                        ),
                    )
                    .await
                    .map_err(map_management_build_error)?,
                ),
                Some((_, None, _, _, _, _)) => {
                    return Err(AppError::new(
                        AppErrorKind::Prepare,
                        "Management Server 需要文件形式的启动配置",
                    ));
                }
                None => None,
            };
            let mut service =
                DnsService::with_default_timeout_from_coordinator_storage_and_telemetry(
                    coordinator,
                    storage,
                    telemetry,
                )
                .map_err(map_service_start_error)?;
            service
                .attach_logging(logging)
                .map_err(map_service_start_error)?;
            if let Some(management) = management {
                service
                    .attach_management(management)
                    .map_err(map_service_start_error)?;
            }
            tracing::info!(
                event = "service_ready",
                component = "application",
                result = "success",
                listener_count = service.runtime().listeners().len(),
                task_count = service.task_count(),
                "service_ready"
            );
            let config_watcher = watched_paths.map(|(source, derived)| {
                Arc::new(tokio::sync::Mutex::new(ConfigFileWatcher::new(
                    source, derived,
                )))
            });
            let polling_watcher = config_watcher.clone();
            let service_result = service
                .wait_for_ctrl_c_with_reload(
                    SHUTDOWN_GRACE_PERIOD,
                    CONFIG_OBSERVATION_POLL_INTERVAL,
                    move |_service| {
                        let watcher = polling_watcher.clone();
                        Box::pin(async move {
                            if let Some(watcher) = watcher {
                                report_config_files(&watcher).await;
                            }
                            Ok(())
                        })
                    },
                )
                .await;
            if let Some(watcher) = config_watcher
                && !watcher.lock().await.finish(SHUTDOWN_GRACE_PERIOD).await
            {
                tracing::warn!(
                    event = "configuration_observation_shutdown_incomplete",
                    component = "application",
                    result = "read_task_not_confirmed_stopped",
                    "configuration_observation_shutdown_incomplete"
                );
            }
            service_result.map_err(map_service_error)?;
            tracing::info!(
                event = "service_shutdown",
                component = "application",
                result = "success",
                "service_shutdown"
            );
            Ok(())
        }
    }
}

fn map_bind_error(error: crate::runtime::BindError) -> AppError {
    AppError::new(AppErrorKind::BindOrStartup, bounded_message(error))
}

fn map_service_start_error(error: ServiceStartError) -> AppError {
    AppError::new(AppErrorKind::BindOrStartup, bounded_message(error))
}

fn map_management_build_error(error: crate::management::ManagementBuildError) -> AppError {
    AppError::new(AppErrorKind::BindOrStartup, bounded_message(error))
}

fn map_storage_prepare_error(error: crate::storage::StorageRuntimeBuildError) -> AppError {
    AppError::new(AppErrorKind::Prepare, bounded_message(error))
}

fn map_service_error(error: ServiceError) -> AppError {
    let kind = match &error {
        ServiceError::Signal
        | ServiceError::TaskFailure { .. }
        | ServiceError::Storage { .. }
        | ServiceError::Telemetry { .. } => AppErrorKind::RuntimeFatal,
        ServiceError::ShutdownDeadline => AppErrorKind::ShutdownTimeout,
    };
    AppError::new(kind, bounded_message(error))
}

fn map_config_error(error: ConfigLoadError) -> AppError {
    let kind = match &error {
        ConfigLoadError::SnapshotConflict { .. }
        | ConfigLoadError::SnapshotSymlink { .. }
        | ConfigLoadError::SnapshotIo { .. }
        | ConfigLoadError::RelativeWorkPath { .. } => AppErrorKind::Prepare,
        _ => AppErrorKind::CliOrConfig,
    };
    AppError::new(kind, bounded_message(error))
}

fn bounded_message(error: impl fmt::Display) -> String {
    const MAX_ERROR_MESSAGE_BYTES: usize = 512;
    let mut message = error.to_string();
    if message.len() > MAX_ERROR_MESSAGE_BYTES {
        let mut end = MAX_ERROR_MESSAGE_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str("...");
    }
    message
}

fn print_help() {
    println!(
        "用法：fluxdns [run|validate] [--config PATH]\n\n命令：\n  run       加载配置、绑定 listener 并运行 DNS 服务，直到收到 Ctrl-C\n  validate  只读加载并校验配置，不创建配置快照或监听端口\n\n选项：\n  -c, --config PATH  配置文件路径（默认：config.yaml）\n  -h, --help         显示帮助\n  -V, --version      显示版本"
    );
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::time::{Duration, Instant};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, RecordType};
    use tokio::net::UdpSocket;

    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::{Cancellation, Deadline, RuntimeRevision};
    use crate::ports::effects::{
        ActivatedSocket, ActivatedSocketHandle, PreparedSocket, SocketFactory, SocketKind,
        SocketSpec,
    };
    use crate::ports::{PortError, PortErrorClass, PortFuture};
    use crate::runtime::{PreparedRuntime, RuntimeCoordinator};
    use crate::service::DnsService;

    use super::{
        AppCommand, AppErrorKind, AppExitCode, ApplicationReloadError, CliAction, CliError,
        parse_args, reload_runtime_from_path, reload_service_from_path,
    };

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

    pub(super) fn reload_source(work: &std::path::Path, port: u16) -> String {
        format!(
            r#"
version: 1
work:
  path: {}
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
    port: {}
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
"#,
            work.display(),
            port,
        )
    }

    pub(super) async fn udp_query(address: SocketAddr, id: u16, name: &str) -> Message {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut query = Message::new(id, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
        socket
            .send_to(&query.to_vec().unwrap(), address)
            .await
            .unwrap();
        let mut response = [0_u8; 4096];
        let (size, _) =
            tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut response))
                .await
                .unwrap()
                .unwrap();
        Message::from_vec(&response[..size]).unwrap()
    }

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(AppExitCode::Success.value(), 0);
        assert_eq!(AppErrorKind::CliOrConfig.exit_code().value(), 2);
        assert_eq!(AppErrorKind::Prepare.exit_code().value(), 3);
        assert_eq!(AppErrorKind::BindOrStartup.exit_code().value(), 4);
        assert_eq!(AppErrorKind::RuntimeFatal.exit_code().value(), 5);
        assert_eq!(AppErrorKind::ShutdownTimeout.exit_code().value(), 5);
    }

    #[test]
    fn cli_defaults_to_run_and_config_yaml() {
        let (action, options) = parse_args(Vec::<OsString>::new()).unwrap();
        assert_eq!(action, CliAction::Command);
        assert_eq!(options.command, AppCommand::Run);
        assert_eq!(options.config_path, std::path::Path::new("config.yaml"));
    }

    #[test]
    fn cli_supports_validate_and_explicit_config_path() {
        let (action, options) = parse_args([
            OsString::from("validate"),
            OsString::from("--config"),
            OsString::from("/tmp/fluxdns.yaml"),
        ])
        .unwrap();
        assert_eq!(action, CliAction::Command);
        assert_eq!(options.command, AppCommand::Validate);
        assert_eq!(
            options.config_path,
            std::path::Path::new("/tmp/fluxdns.yaml")
        );
    }

    #[test]
    fn cli_rejects_ambiguous_or_unknown_arguments() {
        assert_eq!(
            parse_args([OsString::from("run"), OsString::from("validate")]),
            Err(CliError::MultipleCommands)
        );
        assert_eq!(
            parse_args([OsString::from("--unknown")]),
            Err(CliError::UnknownArgument)
        );
        assert_eq!(
            parse_args([OsString::from("--config")]),
            Err(CliError::MissingValue)
        );
    }

    #[tokio::test]
    async fn validate_command_does_not_require_secret_values_or_snapshot_writes() {
        let root =
            std::env::temp_dir().join(format!("fluxdns-app-validate-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let work = root.join("work");
        let path = root.join("input.yaml");
        let source = include_str!("../../config-example.yaml")
            .replace("path: /etc/fluxdns", &format!("path: {}", work.display()));
        std::fs::write(&path, source).unwrap();

        let result = super::run_with_args([
            OsString::from("validate"),
            OsString::from("--config"),
            path.clone().into_os_string(),
        ])
        .await;

        assert!(result.is_ok());
        assert!(!work.join("config.yaml").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reload_loads_prepares_binds_and_activates_next_revision_without_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-app-reload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reload.yaml");
        std::fs::write(&path, reload_source(&root, 5300)).unwrap();

        let initial = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_from_path(&path)
            .unwrap()
            .resolved;
        let prepared = PreparedRuntime::prepare(initial, RuntimeRevision(1)).unwrap();
        let bind_cancellation = Cancellation::new();
        let candidate = crate::runtime::bind_prepared(
            prepared,
            &TestSocketFactory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &bind_cancellation,
        )
        .await
        .unwrap();
        let coordinator = RuntimeCoordinator::new(candidate);
        let previous = coordinator.load();

        std::fs::write(&path, reload_source(&root, 5301)).unwrap();
        let active = reload_runtime_from_path(
            &coordinator,
            &path,
            &TestSocketFactory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(active.revision(), RuntimeRevision(2));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(2));
        assert!(previous.is_draining());
        assert_eq!(active.listeners().local_addrs().unwrap()[0].port(), 5301);
        assert!(!root.join("config.yaml").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reload_failure_keeps_the_current_runtime_active() {
        let root =
            std::env::temp_dir().join(format!("fluxdns-app-reload-failure-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reload.yaml");
        std::fs::write(&path, reload_source(&root, 5300)).unwrap();

        let initial = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_from_path(&path)
            .unwrap()
            .resolved;
        let prepared = PreparedRuntime::prepare(initial, RuntimeRevision(1)).unwrap();
        let bind_cancellation = Cancellation::new();
        let candidate = crate::runtime::bind_prepared(
            prepared,
            &TestSocketFactory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &bind_cancellation,
        )
        .await
        .unwrap();
        let coordinator = RuntimeCoordinator::new(candidate);
        let current = coordinator.load();
        std::fs::write(&path, "version: 1\n").unwrap();

        let error = reload_runtime_from_path(
            &coordinator,
            &path,
            &TestSocketFactory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ApplicationReloadError::Config(_)));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
        assert!(!current.is_draining());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_reload_rejects_process_owned_change_before_prepare() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-app-reload-restart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reload.yaml");
        std::fs::write(&path, reload_source(&root, 5300)).unwrap();
        let initial = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_from_path(&path)
            .unwrap()
            .resolved;
        let prepared = PreparedRuntime::prepare(initial, RuntimeRevision(1)).unwrap();
        let candidate = crate::runtime::bind_prepared(
            prepared,
            &TestSocketFactory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = RuntimeCoordinator::new(candidate);
        let current = coordinator.load();

        let changed =
            reload_source(&root, 5301).replace("path: ./data.sqlite", "path: ./other.sqlite");
        std::fs::write(&path, changed).unwrap();
        let error = reload_runtime_from_path(
            &coordinator,
            &path,
            &TestSocketFactory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            ApplicationReloadError::RestartRequired {
                component: "database"
            }
        ));
        assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
        assert!(!current.is_draining());
        assert!(!root.join("other.sqlite").exists());
        std::fs::write(
            &path,
            reload_source(&root, 5300).replace("level: info", "level: debug"),
        )
        .unwrap();
        let error = reload_runtime_from_path(
            &coordinator,
            &path,
            &TestSocketFactory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            ApplicationReloadError::Service(crate::service::ServiceReloadError::Logging(
                crate::observability::LoggingError::Unavailable
            ))
        ));
        assert!(std::sync::Arc::ptr_eq(&current, &coordinator.load()) && !current.is_draining());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn service_reload_api_switches_listener_and_policy_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "fluxdns-app-service-reload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reload.yaml");
        let initial_port = 44_000 + (std::process::id() as u16 % 400) * 2;
        std::fs::write(&path, reload_source(&root, initial_port)).unwrap();

        let initial = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_from_path(&path)
            .unwrap()
            .resolved;
        let prepared =
            PreparedRuntime::prepare_with_policy_core(initial, RuntimeRevision(1)).unwrap();
        let factory = crate::runtime::SystemSocketFactory::new();
        let candidate = crate::runtime::bind_prepared(
            prepared,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        let coordinator = std::sync::Arc::new(RuntimeCoordinator::new(candidate));
        let mut service =
            DnsService::with_default_timeout_from_coordinator(std::sync::Arc::clone(&coordinator))
                .unwrap();
        let initial_address = service.runtime().listeners().local_addrs().unwrap()[0];
        let initial_response = udp_query(initial_address, 1, "example.test.").await;
        assert_eq!(
            initial_response.metadata.response_code,
            ResponseCode::NoError
        );
        assert!(initial_response.answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0 == Ipv4Addr::new(127, 0, 0, 1)
        )));

        let updated = reload_source(&root, initial_port + 1)
            .replace("127.0.0.1 example.test", "127.0.0.2 example.test");
        std::fs::write(&path, updated).unwrap();
        let active = reload_service_from_path(
            &mut service,
            &path,
            &factory,
            Deadline::new(Instant::now() + Duration::from_secs(5)),
            Cancellation::new(),
        )
        .await
        .unwrap();
        assert_eq!(active.revision(), RuntimeRevision(2));
        let updated_address = active.listeners().local_addrs().unwrap()[0];
        assert_eq!(updated_address.port(), initial_port + 1);

        let updated_response = udp_query(updated_address, 2, "example.test.").await;
        assert_eq!(
            updated_response.metadata.response_code,
            ResponseCode::NoError
        );
        assert!(updated_response.answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0 == Ipv4Addr::new(127, 0, 0, 2)
        )));

        let report = service
            .shutdown(
                &crate::runtime::SystemClock::new(),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert!(!report.deadline_expired);
        let _ = std::fs::remove_dir_all(root);
    }
}
