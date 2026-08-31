//! 基于 `socket2` 的系统 socket adapter。
//!
//! Runtime 只负责准备、激活和持有句柄；Transport 通过 `ports::effects` 中的
//! 协议无关 trait 使用已激活 socket，不直接依赖本模块的 Tokio 类型。

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::{
    ActivatedSocket, ActivatedSocketHandle, PreparedSocket, SocketFactory, SocketKind, SocketSpec,
    TcpConnectionHandle, TcpListenerHandle, TcpReadChunkResult, TcpReadResult, UdpDatagram,
    UdpSocketHandle,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

const TCP_BACKLOG: i32 = 128;

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSocketFactory;

impl SystemSocketFactory {
    pub const fn new() -> Self {
        Self
    }
}

struct SystemPreparedSocket {
    spec: SocketSpec,
    socket: Option<Socket>,
}

struct SystemActivatedSocket {
    kind: SocketKind,
    address: SocketAddr,
    handle: ActivatedSocketHandle,
}

struct TokioUdpSocket {
    socket: UdpSocket,
}

struct TokioTcpListener {
    listener: TcpListener,
}

struct TokioTcpConnection {
    stream: TcpStream,
}

impl SocketFactory for SystemSocketFactory {
    fn prepare<'a>(
        &'a self,
        spec: SocketSpec,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Box<dyn PreparedSocket>, PortError>> {
        Box::pin(async move {
            check_budget(deadline, cancellation, "system_socket.prepare")?;
            let socket = create_socket(spec)?;
            check_budget(deadline, cancellation, "system_socket.prepare")?;
            Ok(Box::new(SystemPreparedSocket {
                spec,
                socket: Some(socket),
            }) as Box<dyn PreparedSocket>)
        })
    }
}

impl PreparedSocket for SystemPreparedSocket {
    fn local_addr(&self) -> Result<SocketAddr, PortError> {
        self.socket
            .as_ref()
            .ok_or_else(|| PortError::new(PortErrorClass::Internal, "system_socket.local_addr"))
            .and_then(socket_local_addr)
    }

    fn activate(mut self: Box<Self>) -> Result<Box<dyn ActivatedSocket>, PortError> {
        let socket = self
            .socket
            .take()
            .ok_or_else(|| PortError::new(PortErrorClass::Internal, "system_socket.activate"))?;
        socket
            .set_nonblocking(true)
            .map_err(|error| map_io(error, "system_socket.set_nonblocking"))?;
        let address = socket_local_addr(&socket)?;
        let handle = match self.spec.kind {
            SocketKind::Udp => {
                let socket: std::net::UdpSocket = socket.into();
                let socket = UdpSocket::from_std(socket)
                    .map_err(|error| map_io(error, "system_socket.udp_from_std"))?;
                ActivatedSocketHandle::Udp(Arc::new(TokioUdpSocket { socket }))
            }
            SocketKind::Tcp => {
                socket
                    .listen(TCP_BACKLOG)
                    .map_err(|error| map_io(error, "system_socket.listen"))?;
                let listener: std::net::TcpListener = socket.into();
                let listener = TcpListener::from_std(listener)
                    .map_err(|error| map_io(error, "system_socket.tcp_from_std"))?;
                ActivatedSocketHandle::Tcp(Arc::new(TokioTcpListener { listener }))
            }
        };
        Ok(Box::new(SystemActivatedSocket {
            kind: self.spec.kind,
            address,
            handle,
        }))
    }
}

impl ActivatedSocket for SystemActivatedSocket {
    fn local_addr(&self) -> Result<SocketAddr, PortError> {
        Ok(self.address)
    }

    fn kind(&self) -> SocketKind {
        self.kind
    }

    fn socket_handle(&self) -> Result<ActivatedSocketHandle, PortError> {
        Ok(self.handle.clone())
    }
}

impl UdpSocketHandle for TokioUdpSocket {
    fn local_addr(&self) -> Result<SocketAddr, PortError> {
        self.socket
            .local_addr()
            .map_err(|error| map_io(error, "system_socket.udp_local_addr"))
    }

    fn recv_from<'a>(
        &'a self,
        max_bytes: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<UdpDatagram, PortError>> {
        Box::pin(async move {
            if max_bytes == 0 {
                return Err(PortError::new(
                    PortErrorClass::InvalidInput,
                    "system_socket.udp_recv_from",
                ));
            }
            let mut buffer = vec![0_u8; max_bytes];
            let (length, peer) = await_io(
                self.socket.recv_from(&mut buffer),
                deadline,
                cancellation,
                "system_socket.udp_recv_from",
            )
            .await?;
            buffer.truncate(length);
            Ok(UdpDatagram {
                payload: buffer,
                peer,
            })
        })
    }

    fn send_to<'a>(
        &'a self,
        payload: Vec<u8>,
        target: SocketAddr,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let length = await_io(
                self.socket.send_to(&payload, target),
                deadline,
                cancellation,
                "system_socket.udp_send_to",
            )
            .await?;
            if length != payload.len() {
                return Err(PortError::new(
                    PortErrorClass::ProtocolViolation,
                    "system_socket.udp_send_to",
                ));
            }
            Ok(())
        })
    }
}

impl TcpListenerHandle for TokioTcpListener {
    fn local_addr(&self) -> Result<SocketAddr, PortError> {
        self.listener
            .local_addr()
            .map_err(|error| map_io(error, "system_socket.tcp_local_addr"))
    }

    fn accept<'a>(
        &'a self,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<Box<dyn TcpConnectionHandle>>, PortError>> {
        Box::pin(async move {
            let (stream, _address) = await_io(
                self.listener.accept(),
                deadline,
                cancellation,
                "system_socket.tcp_accept",
            )
            .await?;
            Ok(Some(
                Box::new(TokioTcpConnection { stream }) as Box<dyn TcpConnectionHandle>
            ))
        })
    }
}

impl TcpConnectionHandle for TokioTcpConnection {
    fn peer_addr(&self) -> Result<SocketAddr, PortError> {
        self.stream
            .peer_addr()
            .map_err(|error| map_io(error, "system_socket.tcp_peer_addr"))
    }

    fn read_exact<'a>(
        &'a mut self,
        length: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<TcpReadResult, PortError>> {
        Box::pin(async move {
            if length > u16::MAX as usize {
                return Err(PortError::new(
                    PortErrorClass::ResourceExhausted,
                    "system_socket.tcp_read_exact",
                ));
            }
            let mut buffer = vec![0_u8; length];
            let mut offset = 0;
            while offset < length {
                let count = await_io(
                    self.stream.read(&mut buffer[offset..]),
                    deadline,
                    cancellation,
                    "system_socket.tcp_read_exact",
                )
                .await?;
                if count == 0 {
                    if offset == 0 {
                        return Ok(TcpReadResult::CleanEof);
                    }
                    return Err(PortError::new(
                        PortErrorClass::ProtocolViolation,
                        "system_socket.tcp_read_exact",
                    ));
                }
                offset += count;
            }
            Ok(TcpReadResult::Complete(buffer))
        })
    }

    fn read_chunk<'a>(
        &'a mut self,
        max_bytes: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<TcpReadChunkResult, PortError>> {
        Box::pin(async move {
            if max_bytes == 0 {
                return Err(PortError::new(
                    PortErrorClass::InvalidInput,
                    "system_socket.tcp_read_chunk",
                ));
            }
            let mut buffer = vec![0_u8; max_bytes];
            let count = await_io(
                self.stream.read(&mut buffer),
                deadline,
                cancellation,
                "system_socket.tcp_read_chunk",
            )
            .await?;
            if count == 0 {
                return Ok(TcpReadChunkResult::CleanEof);
            }
            buffer.truncate(count);
            Ok(TcpReadChunkResult::Data(buffer))
        })
    }

    fn write_all<'a>(
        &'a mut self,
        payload: Vec<u8>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            await_io(
                self.stream.write_all(&payload),
                deadline,
                cancellation,
                "system_socket.tcp_write",
            )
            .await
        })
    }

    fn shutdown(&mut self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            self.stream
                .shutdown()
                .await
                .map_err(|error| map_io(error, "system_socket.tcp_shutdown"))
        })
    }
}

async fn await_io<'a, F, T>(
    future: F,
    deadline: Deadline,
    cancellation: &'a Cancellation,
    operation: &'static str,
) -> Result<T, PortError>
where
    F: Future<Output = io::Result<T>> + Send + 'a,
    T: Send + 'a,
{
    tokio::select! {
        result = future => result.map_err(|error| map_io(error, operation)),
        _ = cancellation.cancelled() => Err(PortError::new(
            PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::Shutdown)),
            operation,
        )),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.at())) => {
            Err(PortError::new(PortErrorClass::Timeout, operation))
        }
    }
}

fn create_socket(spec: SocketSpec) -> Result<Socket, PortError> {
    let domain = match spec.address {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let (kind, protocol) = match spec.kind {
        SocketKind::Udp => (Type::DGRAM, Protocol::UDP),
        SocketKind::Tcp => (Type::STREAM, Protocol::TCP),
    };
    let socket = Socket::new(domain, kind, Some(protocol))
        .map_err(|error| map_io(error, "system_socket.create"))?;
    socket
        .set_reuse_address(true)
        .map_err(|error| map_io(error, "system_socket.reuse_address"))?;
    configure_reuse_port(&socket, spec.reuse_port)?;
    if spec.address.is_ipv6() {
        socket
            .set_only_v6(spec.v6_only)
            .map_err(|error| map_io(error, "system_socket.v6_only"))?;
    }
    socket
        .bind(&spec.address.into())
        .map_err(|error| map_io(error, "system_socket.bind"))?;
    Ok(socket)
}

#[cfg(unix)]
fn configure_reuse_port(socket: &Socket, enabled: bool) -> Result<(), PortError> {
    socket
        .set_reuse_port(enabled)
        .map_err(|error| map_io(error, "system_socket.reuse_port"))
}

#[cfg(not(unix))]
fn configure_reuse_port(_socket: &Socket, _enabled: bool) -> Result<(), PortError> {
    Ok(())
}

fn socket_local_addr(socket: &Socket) -> Result<SocketAddr, PortError> {
    socket
        .local_addr()
        .map_err(|error| map_io(error, "system_socket.local_addr"))?
        .as_socket()
        .ok_or_else(|| PortError::new(PortErrorClass::Internal, "system_socket.local_addr"))
}

fn check_budget(
    deadline: Deadline,
    cancellation: &Cancellation,
    operation: &'static str,
) -> Result<(), PortError> {
    if cancellation.is_cancelled() {
        return Err(PortError::new(
            PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::Shutdown)),
            operation,
        ));
    }
    if deadline.is_expired(std::time::Instant::now()) {
        return Err(PortError::new(PortErrorClass::Timeout, operation));
    }
    Ok(())
}

fn map_io(error: io::Error, operation: &'static str) -> PortError {
    let class = match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::AddrNotAvailable => {
            PortErrorClass::InvalidInput
        }
        io::ErrorKind::PermissionDenied => PortErrorClass::PermissionDenied,
        io::ErrorKind::TimedOut => PortErrorClass::Timeout,
        io::ErrorKind::WouldBlock | io::ErrorKind::ConnectionRefused => PortErrorClass::Unavailable,
        _ => PortErrorClass::Unavailable,
    };
    PortError::new(class, operation)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::{Duration, Instant};

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    use crate::dns::{Cancellation, Deadline};
    use crate::ports::effects::{
        ActivatedSocketHandle, SocketFactory, SocketKind, TcpReadChunkResult,
    };

    use super::SystemSocketFactory;

    #[tokio::test]
    async fn udp_socket_is_prepared_then_activated_with_an_opaque_handle() {
        let factory = SystemSocketFactory::new();
        let cancellation = Cancellation::new();
        let prepared = factory
            .prepare(
                crate::ports::effects::SocketSpec {
                    kind: SocketKind::Udp,
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                    reuse_port: false,
                    v6_only: false,
                },
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        let local = prepared.local_addr().unwrap();
        assert_eq!(local.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let activated = prepared.activate().unwrap();
        assert_eq!(activated.kind(), SocketKind::Udp);
        let handle = activated.socket_handle().unwrap();
        assert!(matches!(handle, ActivatedSocketHandle::Udp(_)));
        let ActivatedSocketHandle::Udp(socket) = handle else {
            unreachable!();
        };
        assert_eq!(socket.local_addr().unwrap(), local);
    }

    #[tokio::test]
    async fn tcp_socket_is_listening_only_after_activation() {
        let factory = SystemSocketFactory::new();
        let cancellation = Cancellation::new();
        let prepared = factory
            .prepare(
                crate::ports::effects::SocketSpec {
                    kind: SocketKind::Tcp,
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                    reuse_port: false,
                    v6_only: false,
                },
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        let local = prepared.local_addr().unwrap();
        let activated = prepared.activate().unwrap();
        let ActivatedSocketHandle::Tcp(listener) = activated.socket_handle().unwrap() else {
            unreachable!();
        };
        assert_eq!(listener.local_addr().unwrap(), local);
    }

    #[tokio::test]
    async fn tcp_read_chunk_is_bounded_and_preserves_bytes() {
        let factory = SystemSocketFactory::new();
        let cancellation = Cancellation::new();
        let prepared = factory
            .prepare(
                crate::ports::effects::SocketSpec {
                    kind: SocketKind::Tcp,
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                    reuse_port: false,
                    v6_only: false,
                },
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        let activated = prepared.activate().unwrap();
        let ActivatedSocketHandle::Tcp(listener) = activated.socket_handle().unwrap() else {
            unreachable!();
        };
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        client.write_all(b"GET /dns-query").await.unwrap();
        let mut connection = listener
            .accept(
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap()
            .unwrap();

        let result = connection
            .read_chunk(
                4,
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(result, TcpReadChunkResult::Data(b"GET ".to_vec()));
    }

    #[tokio::test]
    async fn tcp_read_chunk_reports_clean_eof() {
        let factory = SystemSocketFactory::new();
        let cancellation = Cancellation::new();
        let prepared = factory
            .prepare(
                crate::ports::effects::SocketSpec {
                    kind: SocketKind::Tcp,
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                    reuse_port: false,
                    v6_only: false,
                },
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        let activated = prepared.activate().unwrap();
        let ActivatedSocketHandle::Tcp(listener) = activated.socket_handle().unwrap() else {
            unreachable!();
        };
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let mut connection = listener
            .accept(
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap()
            .unwrap();
        drop(client);

        let result = connection
            .read_chunk(
                8,
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(result, TcpReadChunkResult::CleanEof);
    }

    #[tokio::test]
    async fn tcp_read_chunk_observes_cancellation() {
        let factory = SystemSocketFactory::new();
        let cancellation = Cancellation::new();
        let prepared = factory
            .prepare(
                crate::ports::effects::SocketSpec {
                    kind: SocketKind::Tcp,
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                    reuse_port: false,
                    v6_only: false,
                },
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        let activated = prepared.activate().unwrap();
        let ActivatedSocketHandle::Tcp(listener) = activated.socket_handle().unwrap() else {
            unreachable!();
        };
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let mut connection = listener
            .accept(
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap()
            .unwrap();
        let read = connection.read_chunk(
            8,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
            &cancellation,
        );
        cancellation.cancel(crate::dns::CancelReason::Shutdown);
        let error = read.await.unwrap_err();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::Cancelled(crate::dns::CancelReason::Shutdown)
        ));
    }

    #[tokio::test]
    async fn tcp_read_chunk_rejects_zero_limit_and_observes_deadline() {
        let factory = SystemSocketFactory::new();
        let cancellation = Cancellation::new();
        let prepared = factory
            .prepare(
                crate::ports::effects::SocketSpec {
                    kind: SocketKind::Tcp,
                    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                    reuse_port: false,
                    v6_only: false,
                },
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        let activated = prepared.activate().unwrap();
        let ActivatedSocketHandle::Tcp(listener) = activated.socket_handle().unwrap() else {
            unreachable!();
        };
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let mut connection = listener
            .accept(
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap()
            .unwrap();

        let invalid = connection
            .read_chunk(
                0,
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            invalid.class(),
            crate::ports::PortErrorClass::InvalidInput
        ));

        let timeout = connection
            .read_chunk(
                8,
                Deadline::new(Instant::now() + Duration::from_millis(20)),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            timeout.class(),
            crate::ports::PortErrorClass::Timeout
        ));
    }
}
