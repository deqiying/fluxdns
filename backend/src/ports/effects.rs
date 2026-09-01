//! 时间、资源、secret 与 socket 准备等受约束副作用。

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::dns::{Cancellation, Deadline};

use super::{PortError, PortFuture};

pub trait Clock: Send + Sync {
    fn monotonic_now(&self) -> Instant;

    fn utc_now(&self) -> SystemTime;

    fn sleep_until(&self, deadline: Deadline) -> PortFuture<'_, ()>;
}

#[derive(Clone)]
pub struct ResourceLocation(Arc<str>);

impl ResourceLocation {
    pub fn new(value: impl Into<Arc<str>>) -> Result<Self, PortError> {
        let value = value.into();
        if value.is_empty() {
            return Err(PortError::new(
                super::PortErrorClass::InvalidInput,
                "resource_location",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ResourceLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceLocation")
            .field("byte_len", &self.0.len())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProxyProfileId(pub Arc<str>);

#[derive(Clone)]
pub struct ResourceFetchRequest {
    pub location: ResourceLocation,
    pub proxy_profile: Option<ProxyProfileId>,
    pub max_bytes: usize,
    pub deadline: Deadline,
    pub cancellation: Cancellation,
}

impl fmt::Debug for ResourceFetchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceFetchRequest")
            .field("location_byte_len", &self.location.0.len())
            .field("has_proxy_profile", &self.proxy_profile.is_some())
            .field("max_bytes", &self.max_bytes)
            .field("deadline", &self.deadline)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish()
    }
}

#[derive(Clone)]
pub struct ResourceFetchResult {
    pub body: Arc<[u8]>,
    pub checksum: u64,
    pub modified_at: Option<SystemTime>,
}

impl fmt::Debug for ResourceFetchResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceFetchResult")
            .field("body_len", &self.body.len())
            .field("checksum", &self.checksum)
            .field("modified_at", &self.modified_at)
            .finish()
    }
}

pub trait ResourceFetcher: Send + Sync {
    fn fetch(
        &self,
        request: ResourceFetchRequest,
    ) -> PortFuture<'_, Result<ResourceFetchResult, PortError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretReference {
    Environment(Arc<str>),
    File(PathBuf),
}

/// secret 值的 `Debug`/`Display` 固定脱敏，且不实现序列化 trait。
pub struct SecretValue(Box<[u8]>);

impl SecretValue {
    pub fn new(value: impl Into<Box<[u8]>>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

pub trait SecretProvider: Send + Sync {
    fn resolve<'a>(
        &'a self,
        reference: &'a SecretReference,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<SecretValue, PortError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketKind {
    Udp,
    Tcp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketSpec {
    pub kind: SocketKind,
    pub address: SocketAddr,
    pub reuse_port: bool,
    pub v6_only: bool,
}

/// UDP 收到的数据报；payload 由 adapter 拥有，避免把 buffer lifetime 暴露给核心层。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UdpDatagram {
    pub payload: Vec<u8>,
    pub peer: SocketAddr,
}

/// 已激活 UDP socket 的协议无关操作；具体 Tokio/std 类型只存在于 adapter 内部。
pub trait UdpSocketHandle: Send + Sync {
    fn local_addr(&self) -> Result<SocketAddr, PortError>;

    fn recv_from<'a>(
        &'a self,
        max_bytes: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<UdpDatagram, PortError>>;

    fn send_to<'a>(
        &'a self,
        payload: Vec<u8>,
        target: SocketAddr,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>>;
}

/// 已接受 TCP 连接的协议无关操作。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TcpReadResult {
    /// Requested bytes were read in full.
    Complete(Vec<u8>),
    /// The peer closed the connection before a new frame started.
    CleanEof,
}

/// Opaque TCP byte-stream read result for protocols without message framing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TcpReadChunkResult {
    /// Up to the requested number of bytes were read.
    Data(Vec<u8>),
    /// The peer closed the connection before any more bytes were available.
    CleanEof,
}

pub trait TcpConnectionHandle: Send {
    fn peer_addr(&self) -> Result<SocketAddr, PortError>;

    fn read_exact<'a>(
        &'a mut self,
        length: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<TcpReadResult, PortError>>;

    /// Read one bounded byte-stream chunk. Implementations that only support
    /// DNS framing keep the compatibility default until they opt into this API.
    fn read_chunk<'a>(
        &'a mut self,
        max_bytes: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<TcpReadChunkResult, PortError>> {
        let _ = (max_bytes, deadline, cancellation);
        Box::pin(async {
            Err(PortError::new(
                super::PortErrorClass::Internal,
                "tcp.read_chunk.unsupported",
            ))
        })
    }

    fn write_all<'a>(
        &'a mut self,
        payload: Vec<u8>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>>;

    fn shutdown(&mut self) -> PortFuture<'_, Result<(), PortError>>;
}

/// 已连接 outbound byte stream 的协议无关操作。
///
/// 该 trait 与入站 `TcpConnectionHandle` 分离，避免把连接建立方向和
/// 生命周期混入 listener/session 所有权；具体 dial 仍由后续 adapter 提供。
pub trait OutboundStream: Send {
    fn read_exact<'a>(
        &'a mut self,
        length: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<TcpReadResult, PortError>>;

    fn write_all<'a>(
        &'a mut self,
        payload: Vec<u8>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>>;

    fn shutdown(&mut self) -> PortFuture<'_, Result<(), PortError>>;
}

/// 已激活 TCP listener 的协议无关操作。
pub trait TcpListenerHandle: Send + Sync {
    fn local_addr(&self) -> Result<SocketAddr, PortError>;

    fn accept<'a>(
        &'a self,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<Box<dyn TcpConnectionHandle>>, PortError>>;
}

/// Runtime 交给 Transport 的不透明 socket 句柄。
#[derive(Clone)]
pub enum ActivatedSocketHandle {
    Udp(Arc<dyn UdpSocketHandle>),
    Tcp(Arc<dyn TcpListenerHandle>),
}

impl fmt::Debug for ActivatedSocketHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Udp(_) => "udp",
            Self::Tcp(_) => "tcp",
        };
        formatter
            .debug_tuple("ActivatedSocketHandle")
            .field(&kind)
            .finish()
    }
}

/// 兼容早期 Runtime 测试命名；生产接口使用 `ActivatedSocketHandle`。
pub type SocketHandle = ActivatedSocketHandle;

/// Runtime `BindPlan` 提交前的未激活 socket，不暴露 socket2/Tokio 类型。
pub trait PreparedSocket: Send {
    fn local_addr(&self) -> Result<SocketAddr, PortError>;

    fn activate(self: Box<Self>) -> Result<Box<dyn ActivatedSocket>, PortError>;
}

pub trait ActivatedSocket: Send + Sync {
    fn local_addr(&self) -> Result<SocketAddr, PortError>;

    fn kind(&self) -> SocketKind;

    fn socket_handle(&self) -> Result<ActivatedSocketHandle, PortError>;
}

pub trait SocketFactory: Send + Sync {
    fn prepare<'a>(
        &'a self,
        spec: SocketSpec,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Box<dyn PreparedSocket>, PortError>>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use crate::dns::{Cancellation, Deadline};

    use super::{
        ProxyProfileId, ResourceFetchRequest, ResourceFetchResult, ResourceLocation, SecretValue,
    };

    #[test]
    fn secret_value_debug_and_display_are_redacted() {
        let secret = SecretValue::new(Vec::from("do-not-leak").into_boxed_slice());

        assert_eq!(format!("{secret}"), "[REDACTED]");
        assert_eq!(format!("{secret:?}"), "SecretValue([REDACTED])");
        assert!(!format!("{secret:?}").contains("do-not-leak"));
    }

    #[test]
    fn resource_fetch_debug_only_contains_safe_metadata() {
        let location = ResourceLocation::new(
            "https://user:password@rules.example/private?access_token=do-not-log-this",
        )
        .unwrap();
        let request = ResourceFetchRequest {
            location: location.clone(),
            proxy_profile: Some(ProxyProfileId(Arc::from("private-proxy"))),
            max_bytes: 8 * 1024,
            deadline: Deadline::new(Instant::now()),
            cancellation: Cancellation::new(),
        };
        let result = ResourceFetchResult {
            body: Arc::from(&b"domain example.com\nprivate-rule-body"[..]),
            checksum: 42,
            modified_at: None,
        };

        let location_debug = format!("{location:?}");
        let request_debug = format!("{request:?}");
        let result_debug = format!("{result:?}");

        for secret in [
            "user",
            "password",
            "rules.example",
            "access_token",
            "do-not-log-this",
            "private-proxy",
            "example.com",
            "private-rule-body",
        ] {
            assert!(!location_debug.contains(secret));
            assert!(!request_debug.contains(secret));
            assert!(!result_debug.contains(secret));
        }
        assert!(location_debug.contains("byte_len"));
        assert!(request_debug.contains("location_byte_len"));
        assert!(request_debug.contains("has_proxy_profile: true"));
        assert!(result_debug.contains("body_len"));
        assert!(result_debug.contains("checksum: 42"));
    }
}
