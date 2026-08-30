use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use tokio_util::sync::CancellationToken;

use super::CanonicalQuery;

macro_rules! numeric_id {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(pub $inner);

        impl From<$inner> for $name {
            fn from(value: $inner) -> Self {
                Self(value)
            }
        }
    };
}

numeric_id!(RequestId, u128);
numeric_id!(TraceId, u128);
numeric_id!(ConnectionId, u64);
numeric_id!(StreamId, u64);
numeric_id!(RuntimeRevision, u64);
numeric_id!(CacheCompatibilityKey, u64);

macro_rules! named_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(pub String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

named_id!(ListenerId);
named_id!(RouteId);

/// 可能来自 URL path 等不可信入口的客户端标识。
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientId(String);

impl ClientId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ClientId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ClientId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Debug for ClientId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClientId(<redacted>)")
    }
}

/// transport 恢复出的客户端身份；Debug 只显示字段是否存在。
#[derive(Clone, Default, Eq, Hash, PartialEq)]
pub struct ClientIdentity {
    pub peer_addr: Option<SocketAddr>,
    pub client_addr: Option<IpAddr>,
    pub client_id: Option<ClientId>,
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientIdentity")
            .field("peer_addr", &self.peer_addr.as_ref().map(|_| "<redacted>"))
            .field(
                "client_addr",
                &self.client_addr.as_ref().map(|_| "<redacted>"),
            )
            .field("client_id", &self.client_id)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransportClass {
    Datagram,
    Stream,
    Multiplexed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TransportCapabilities {
    pub class: TransportClass,
    pub cache_compatibility: CacheCompatibilityKey,
}

/// 单调时钟上的截止时间，只提供收紧操作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn new(at: Instant) -> Self {
        Self(at)
    }

    pub fn at(self) -> Instant {
        self.0
    }

    pub fn is_expired(self, now: Instant) -> bool {
        now >= self.0
    }

    pub fn remaining(self, now: Instant) -> Duration {
        self.0.saturating_duration_since(now)
    }

    pub fn shorten_to(&mut self, candidate: Instant) {
        self.0 = self.0.min(candidate);
    }

    pub fn shortened_to(self, candidate: Instant) -> Self {
        Self(self.0.min(candidate))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CancelReason {
    ClientDisconnected,
    DeadlineExceeded,
    Shutdown,
    GroupPolicy,
    UpstreamCancelled,
}

/// 保留首个取消原因的协作式取消句柄。
#[derive(Clone)]
pub struct Cancellation {
    token: CancellationToken,
    reason: Arc<OnceLock<CancelReason>>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Arc::new(OnceLock::new()),
        }
    }

    pub fn cancel(&self, reason: CancelReason) {
        let _ = self.reason.set(reason);
        self.token.cancel();
    }

    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    pub fn reason(&self) -> Option<CancelReason> {
        self.reason.get().copied()
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Cancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cancellation")
            .field("is_cancelled", &self.is_cancelled())
            .field("reason", &self.reason())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct RequestMeta {
    pub request_id: RequestId,
    pub trace_id: Option<TraceId>,
    pub received_at: Instant,
    pub received_at_utc: SystemTime,
    pub deadline: Deadline,
    pub cancellation: Cancellation,
    pub connection_id: Option<ConnectionId>,
    pub stream_id: Option<StreamId>,
    pub listener_id: ListenerId,
    pub route_id: Option<RouteId>,
    pub original_dns_id: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub meta: RequestMeta,
    pub client: ClientIdentity,
    pub transport: TransportCapabilities,
    pub runtime_revision: RuntimeRevision,
}

#[derive(Clone, Debug)]
pub struct DnsRequest {
    pub query: CanonicalQuery,
    pub context: RequestContext,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn deadline_can_only_be_shortened() {
        let now = Instant::now();
        let original = now + Duration::from_secs(10);
        let mut deadline = Deadline::new(original);

        deadline.shorten_to(now + Duration::from_secs(20));
        assert_eq!(deadline.at(), original);

        let shorter = now + Duration::from_secs(3);
        deadline.shorten_to(shorter);
        assert_eq!(deadline.at(), shorter);
        assert_eq!(deadline.shortened_to(original).at(), shorter);
    }

    #[tokio::test]
    async fn first_cancellation_reason_wins() {
        let cancellation = Cancellation::new();
        cancellation.cancel(CancelReason::ClientDisconnected);
        cancellation.cancel(CancelReason::Shutdown);
        cancellation.cancelled().await;

        assert!(cancellation.is_cancelled());
        assert_eq!(
            cancellation.reason(),
            Some(CancelReason::ClientDisconnected)
        );
    }

    #[test]
    fn client_debug_output_is_redacted() {
        let client_id = ClientId::new("super-secret-client-id");
        let identity = ClientIdentity {
            peer_addr: Some(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 10), 5353).into()),
            client_addr: Some(Ipv4Addr::new(198, 51, 100, 7).into()),
            client_id: Some(client_id.clone()),
        };

        let client_debug = format!("{client_id:?}");
        let identity_debug = format!("{identity:?}");
        assert!(!client_debug.contains("super-secret-client-id"));
        assert!(!identity_debug.contains("super-secret-client-id"));
        assert!(!identity_debug.contains("192.0.2.10"));
        assert!(!identity_debug.contains("198.51.100.7"));
        assert!(identity_debug.contains("<redacted>"));
    }
}
