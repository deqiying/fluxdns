use std::fmt;

use crate::observability::{Component, EventName, EventResult, TypedEvent};

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
/// `safe_message` 只接受静态文本，避免把配置值、凭据或请求内容意外带到 stderr。
#[derive(Debug, thiserror::Error)]
#[error("{safe_message}")]
pub struct AppError {
    kind: AppErrorKind,
    safe_message: &'static str,
}

impl AppError {
    pub const fn new(kind: AppErrorKind, safe_message: &'static str) -> Self {
        Self { kind, safe_message }
    }

    pub const fn kind(&self) -> AppErrorKind {
        self.kind
    }

    pub const fn safe_message(&self) -> &'static str {
        self.safe_message
    }

    pub const fn exit_code(&self) -> AppExitCode {
        self.kind.exit_code()
    }
}

/// 阶段 1 只验证可运行骨架，不加载配置、绑定端口或启动 DNS runtime。
pub async fn run() -> Result<(), AppError> {
    let event = TypedEvent::new(
        EventName::ScaffoldReady,
        Component::Application,
        EventResult::Success,
        "阶段 1 项目骨架已就绪；DNS 服务尚未启动",
    );

    tracing::info!(
        event = event.name.as_str(),
        component = event.component.as_str(),
        result = event.result.as_str(),
        message = event.message,
        "scaffold_ready"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AppErrorKind, AppExitCode};

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(AppExitCode::Success.value(), 0);
        assert_eq!(AppErrorKind::CliOrConfig.exit_code().value(), 2);
        assert_eq!(AppErrorKind::Prepare.exit_code().value(), 3);
        assert_eq!(AppErrorKind::BindOrStartup.exit_code().value(), 4);
        assert_eq!(AppErrorKind::RuntimeFatal.exit_code().value(), 5);
        assert_eq!(AppErrorKind::ShutdownTimeout.exit_code().value(), 5);
    }
}
