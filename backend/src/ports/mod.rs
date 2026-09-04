//! 核心业务与外部副作用之间的稳定契约。

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::dns::CancelReason;

pub mod cache;
pub mod effects;
pub mod exchange;
pub mod inbound;
pub mod management;
pub mod observation;
pub mod storage;
pub mod telemetry;
pub mod testing;

/// Port 使用的对象安全异步返回值，避免把具体 runtime future 泄漏给核心层。
pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 跨 adapter 保持稳定的错误分类。
pub enum PortErrorClass {
    InvalidInput,
    Timeout,
    Cancelled(CancelReason),
    Unavailable,
    PermissionDenied,
    ResourceExhausted,
    ProtocolViolation,
    CorruptData,
    Internal,
}

impl fmt::Debug for PortErrorClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PortErrorClass {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Timeout => "timeout",
            Self::Cancelled(_) => "cancelled",
            Self::Unavailable => "unavailable",
            Self::PermissionDenied => "permission_denied",
            Self::ResourceExhausted => "resource_exhausted",
            Self::ProtocolViolation => "protocol_violation",
            Self::CorruptData => "corrupt_data",
            Self::Internal => "internal",
        }
    }

    pub const fn cancellation_reason(&self) -> Option<&CancelReason> {
        match self {
            Self::Cancelled(reason) => Some(reason),
            _ => None,
        }
    }
}

/// 可安全跨层格式化的 port 错误。
///
/// adapter 的原始错误文本不得进入这里，避免 credential、URL、query 或 wire
/// 经由 `Display`/`Debug` 意外泄漏。
pub struct PortError {
    class: PortErrorClass,
    operation: &'static str,
    safe_context: Option<&'static str>,
}

impl PortError {
    pub const fn new(class: PortErrorClass, operation: &'static str) -> Self {
        Self {
            class,
            operation,
            safe_context: None,
        }
    }

    pub const fn with_safe_context(mut self, context: &'static str) -> Self {
        self.safe_context = Some(context);
        self
    }

    pub const fn class(&self) -> &PortErrorClass {
        &self.class
    }

    pub const fn operation(&self) -> &'static str {
        self.operation
    }
}

impl fmt::Debug for PortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortError")
            .field("class", &self.class)
            .field("operation", &self.operation)
            .field("safe_context", &self.safe_context)
            .finish()
    }
}

impl fmt::Display for PortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed: {}",
            self.operation,
            self.class.as_str()
        )?;
        if let Some(context) = self.safe_context {
            write!(formatter, " ({context})")?;
        }
        Ok(())
    }
}

impl std::error::Error for PortError {}
