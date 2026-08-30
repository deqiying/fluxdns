use std::{fmt, str::FromStr};

use tracing::Subscriber;

/// 构建仅写 stderr、固定为 INFO 及以上的阶段 1 bootstrap subscriber。
pub fn bootstrap_subscriber() -> impl Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .with_writer(std::io::stderr)
        .finish()
}

/// 安装进程级 bootstrap subscriber。
pub fn init_bootstrap() -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(bootstrap_subscriber())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl FromStr for LogLevel {
    type Err = ParseLogLevelError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.eq_ignore_ascii_case("trace") {
            Ok(Self::Trace)
        } else if input.eq_ignore_ascii_case("debug") {
            Ok(Self::Debug)
        } else if input.eq_ignore_ascii_case("info") {
            Ok(Self::Info)
        } else if input.eq_ignore_ascii_case("warn") {
            Ok(Self::Warn)
        } else if input.eq_ignore_ascii_case("error") {
            Ok(Self::Error)
        } else {
            Err(ParseLogLevelError)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("日志级别必须是 trace、debug、info、warn 或 error")]
pub struct ParseLogLevelError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthState {
    Healthy,
    Degraded,
    Failed,
    Stopping,
}

impl HealthState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Stopping => "stopping",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventName {
    ScaffoldReady,
    ComponentStateChange,
}

impl EventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScaffoldReady => "scaffold_ready",
            Self::ComponentStateChange => "component.state_change",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Component {
    Application,
    Observability,
    DnsCore,
    Ports,
}

impl Component {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Observability => "observability",
            Self::DnsCore => "dns_core",
            Self::Ports => "ports",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventResult {
    Success,
    Degraded,
    Failure,
}

impl EventResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Degraded => "degraded",
            Self::Failure => "failure",
        }
    }
}

/// 阶段 1 的最小 typed event 契约。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TypedEvent {
    pub name: EventName,
    pub component: Component,
    pub result: EventResult,
    pub message: &'static str,
}

impl TypedEvent {
    pub const fn new(
        name: EventName,
        component: Component,
        result: EventResult,
        message: &'static str,
    ) -> Self {
        Self {
            name,
            component,
            result,
            message,
        }
    }
}

/// 包装敏感值，并在所有常规格式化路径中固定脱敏。
#[derive(Clone, Copy, Default)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Sensitive([REDACTED])")
    }
}

impl<T> fmt::Display for Sensitive<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{LogLevel, Sensitive, bootstrap_subscriber};

    #[test]
    fn bootstrap_subscriber_can_be_built() {
        let subscriber = bootstrap_subscriber();
        let _dispatch = tracing::Dispatch::new(subscriber);
    }

    #[test]
    fn log_level_parsing_is_case_insensitive_and_strict() {
        assert_eq!(LogLevel::from_str("trace"), Ok(LogLevel::Trace));
        assert_eq!(LogLevel::from_str("DEBUG"), Ok(LogLevel::Debug));
        assert_eq!(LogLevel::from_str("Info"), Ok(LogLevel::Info));
        assert_eq!(LogLevel::from_str("WARN"), Ok(LogLevel::Warn));
        assert_eq!(LogLevel::from_str("error"), Ok(LogLevel::Error));
        assert!(LogLevel::from_str("warning").is_err());
        assert!(LogLevel::from_str(" info ").is_err());
    }

    #[test]
    fn sensitive_value_never_appears_in_debug_or_display() {
        let secret = "do-not-log-this-value";
        let sensitive = Sensitive::new(secret);
        let debug = format!("{sensitive:?}");
        let display = format!("{sensitive}");
        let derived_debug = format!("{:?}", Some(sensitive));

        assert!(!debug.contains(secret));
        assert!(!display.contains(secret));
        assert!(!derived_debug.contains(secret));
        assert_eq!(debug, "Sensitive([REDACTED])");
        assert_eq!(display, "[REDACTED]");
        assert_eq!(derived_debug, "Some(Sensitive([REDACTED]))");
    }
}
