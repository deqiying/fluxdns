//! 上游 DNS exchange 契约。

use std::fmt;
use std::sync::Arc;

use crate::dns::{CancelReason, CanonicalQuery, CanonicalResponse, RequestContext};

use super::PortFuture;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ConnectorId(Arc<str>);

impl ConnectorId {
    pub fn new(value: impl Into<Arc<str>>) -> Result<Self, ConnectorIdError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err(ConnectorIdError);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(ConnectorIdError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectorIdError;

impl fmt::Display for ConnectorIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("connector id must be a short safe identifier")
    }
}

impl std::error::Error for ConnectorIdError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionPolicy {
    Sequential,
    RoundRobin,
    LoadBalance,
    Parallel,
    Failover,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportFailureClass {
    Connect,
    DnsBootstrap,
    Proxy,
    Tls,
    HttpStatus,
    MediaType,
    BodyLimit,
    Timeout,
    Wire,
    QuestionMismatch,
    Unavailable,
    ProtocolViolation,
    ResourceExhausted,
    Internal,
}

/// 已脱敏的上游传输失败；DNS RCODE 不属于该类型。
#[derive(Debug)]
pub struct TransportFailure {
    pub connector: ConnectorId,
    pub class: TransportFailureClass,
    pub retryable: bool,
    pub safe_context: Option<&'static str>,
}

pub enum UpstreamOutcome {
    Response(CanonicalResponse),
    TransportFailure(TransportFailure),
    Cancelled(CancelReason),
}

impl fmt::Debug for UpstreamOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Response(_) => formatter.write_str("Response(..)"),
            Self::TransportFailure(failure) => formatter
                .debug_tuple("TransportFailure")
                .field(failure)
                .finish(),
            Self::Cancelled(_) => formatter.write_str("Cancelled(..)"),
        }
    }
}

pub trait DnsExchange: Send + Sync {
    /// connector 是已经绑定 profile 的 handle；核心层只能读取稳定 ID。
    fn connector_id(&self) -> &ConnectorId;

    fn exchange<'a>(
        &'a self,
        query: &'a CanonicalQuery,
        context: &'a RequestContext,
    ) -> PortFuture<'a, UpstreamOutcome>;
}
