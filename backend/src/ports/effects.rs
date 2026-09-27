//! 时间、资源与 socket 准备等受约束副作用。

use std::fmt;
use std::net::SocketAddr;
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
    pub validators: ResourceValidators,
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
            .field("validators", &self.validators)
            .finish()
    }
}

/// 远端验证标记是受限不透明字段，Debug 不能回显服务端提供的原文。
#[derive(Clone, Default, Eq, PartialEq)]
pub struct ResourceValidators {
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
}

impl ResourceValidators {
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    /// 拒绝超长或可注入额外 HTTP header 的标记；不自行猜测 ETag 的内容含义。
    pub fn validate(&self) -> Result<(), PortError> {
        for value in [self.etag.as_ref(), self.last_modified.as_ref()]
            .into_iter()
            .flatten()
        {
            if value.is_empty()
                || value.len() > 4096
                || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
            {
                return Err(PortError::new(
                    super::PortErrorClass::ProtocolViolation,
                    "resource_fetch.validator",
                ));
            }
        }
        Ok(())
    }
}

impl fmt::Debug for ResourceValidators {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceValidators")
            .field("has_etag", &self.etag.is_some())
            .field("has_last_modified", &self.last_modified.is_some())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum ResourceFetchResult {
    Modified(ResourceContent),
    NotModified(ResourceValidators),
}

#[derive(Clone)]
pub struct ResourceContent {
    pub body: Arc<[u8]>,
    pub checksum: u64,
    pub modified_at: Option<SystemTime>,
    pub validators: ResourceValidators,
}

impl fmt::Debug for ResourceContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceContent")
            .field("body_len", &self.body.len())
            .field("checksum", &self.checksum)
            .field("modified_at", &self.modified_at)
            .field("validators", &self.validators)
            .finish()
    }
}

pub trait ResourceFetcher: Send + Sync {
    /// 配置/代理解析实例的私有代际；没有代际的 adapter 不跨调用复用条件请求标记。
    fn validator_scope(&self) -> Option<&str> {
        None
    }

    fn fetch(
        &self,
        request: ResourceFetchRequest,
    ) -> PortFuture<'_, Result<ResourceFetchResult, PortError>>;
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

/// 入站 TLS terminate 所需的 DER 材料。
///
/// 该结构只携带已加载到内存的证书和私钥字节，避免把 rustls 类型泄漏到
/// ports；`Debug` 不输出私钥或证书内容。
#[derive(Clone)]
pub struct TlsServerMaterial {
    pub certificate_chain: Vec<Vec<u8>>,
    pub private_key: Vec<u8>,
}

impl fmt::Debug for TlsServerMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsServerMaterial")
            .field("certificate_count", &self.certificate_chain.len())
            .field(
                "certificate_bytes",
                &self.certificate_chain.iter().map(Vec::len).sum::<usize>(),
            )
            .field("private_key_bytes", &self.private_key.len())
            .finish()
    }
}

pub trait TcpConnectionHandle: Send {
    fn peer_addr(&self) -> Result<SocketAddr, PortError>;

    /// 将已接受的 TCP 字节流升级为入站 TLS；不支持升级的实现返回兼容错误。
    fn start_tls<'a>(
        &'a mut self,
        material: Arc<TlsServerMaterial>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        let _ = (material, deadline, cancellation);
        Box::pin(async {
            Err(PortError::new(
                super::PortErrorClass::Unavailable,
                "tcp.start_tls.unsupported",
            ))
        })
    }

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
                "outbound.read_chunk.unsupported",
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

/// 创建 outbound TCP stream 的协议无关 port。
pub trait OutboundDialer: Send + Sync {
    fn connect<'a>(
        &'a self,
        target: SocketAddr,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Box<dyn OutboundStream>, PortError>>;
}

/// 解析 outbound proxy endpoint 的协议无关 port。
pub trait OutboundAddressResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Vec<SocketAddr>, PortError>>;
}

/// 已激活 TCP listener 的协议无关操作。
pub trait TcpListenerHandle: Send + Sync {
    fn local_addr(&self) -> Result<SocketAddr, PortError>;

    fn accept<'a>(
        &'a self,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<Box<dyn TcpConnectionHandle>>, PortError>>;

    /// 接受并完成入站 TLS 握手；不支持 TLS 的 listener 保持兼容默认实现。
    fn accept_with_tls<'a>(
        &'a self,
        material: Arc<TlsServerMaterial>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<Box<dyn TcpConnectionHandle>>, PortError>> {
        let _ = (material, deadline, cancellation);
        Box::pin(async {
            Err(PortError::new(
                super::PortErrorClass::Unavailable,
                "tcp.accept_with_tls.unsupported",
            ))
        })
    }
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
        ProxyProfileId, ResourceContent, ResourceFetchRequest, ResourceFetchResult,
        ResourceLocation, ResourceValidators,
    };

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
            validators: ResourceValidators {
                etag: Some(Arc::from("private-validator")),
                last_modified: None,
            },
        };
        let result = ResourceFetchResult::Modified(ResourceContent {
            body: Arc::from(&b"domain example.com\nprivate-rule-body"[..]),
            checksum: 42,
            modified_at: None,
            validators: request.validators.clone(),
        });

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
            "private-validator",
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
