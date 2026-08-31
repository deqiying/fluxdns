use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{ConfigLoadError, ConfigLoader, LoadOptions};
use crate::dns::{ConfiguredDnsCore, Deadline, RuntimeRevision};
use crate::runtime::SystemSocketFactory;
use crate::service::{DnsService, ServiceError, ServiceStartError};

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

async fn run_command(options: CliOptions) -> Result<(), AppError> {
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
            let prepared =
                crate::runtime::PreparedRuntime::prepare(output.resolved, RuntimeRevision(1))
                    .map_err(|error| {
                        AppError::new(AppErrorKind::Prepare, bounded_message(error))
                    })?;
            tracing::info!(
                event = "runtime_prepared",
                component = "application",
                result = "success",
                revision = prepared.preflight().revision.0,
                endpoint_count = prepared.preflight().endpoint_count,
                "runtime_prepared"
            );
            let bind_cancellation = crate::dns::Cancellation::new();
            let candidate = crate::runtime::bind_prepared(
                prepared,
                &SystemSocketFactory::new(),
                Deadline::new(Instant::now() + BIND_TIMEOUT),
                &bind_cancellation,
            )
            .await
            .map_err(map_bind_error)?;
            let coordinator = crate::runtime::RuntimeCoordinator::new(candidate);
            let active = coordinator.load();
            let core = ConfiguredDnsCore::from_config(active.snapshot().config())
                .map_err(|error| AppError::new(AppErrorKind::Prepare, bounded_message(error)))?;
            let mut service = DnsService::with_default_timeout(active, Arc::new(core))
                .map_err(map_service_start_error)?;
            tracing::info!(
                event = "service_ready",
                component = "application",
                result = "success",
                listener_count = service.runtime().listeners().len(),
                task_count = service.task_count(),
                "service_ready"
            );
            service
                .wait_for_ctrl_c(SHUTDOWN_GRACE_PERIOD)
                .await
                .map_err(map_service_error)?;
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

fn map_service_error(error: ServiceError) -> AppError {
    let kind = match &error {
        ServiceError::Signal => AppErrorKind::RuntimeFatal,
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

    use super::{AppCommand, AppErrorKind, AppExitCode, CliAction, CliError, parse_args};

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
        let source = include_str!("../../config-example.yaml").replace(
            "path: /var/lib/fluxdns",
            &format!("path: {}", work.display()),
        );
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
}
