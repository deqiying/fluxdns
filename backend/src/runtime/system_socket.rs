//! 基于 `socket2` 的系统 socket adapter。
//!
//! Runtime 只负责准备、激活和持有句柄；Transport 通过 `ports::effects` 中的
//! 协议无关 trait 使用已激活 socket，不直接依赖本模块的 Tokio 类型。

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio_rustls::TlsAcceptor;

use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::{
    ActivatedSocket, ActivatedSocketHandle, PreparedSocket, SocketFactory, SocketKind, SocketSpec,
    TcpConnectionHandle, TcpListenerHandle, TcpReadChunkResult, TcpReadResult, TlsServerMaterial,
    UdpDatagram, UdpSocketHandle,
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

trait TokioByteStream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> TokioByteStream for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

struct TokioTcpConnection {
    stream: Option<Box<dyn TokioByteStream>>,
    peer: SocketAddr,
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
            let (stream, address) = await_io(
                self.listener.accept(),
                deadline,
                cancellation,
                "system_socket.tcp_accept",
            )
            .await?;
            Ok(Some(Box::new(TokioTcpConnection {
                stream: Some(Box::new(stream)),
                peer: address,
            }) as Box<dyn TcpConnectionHandle>))
        })
    }

    fn accept_with_tls<'a>(
        &'a self,
        material: Arc<TlsServerMaterial>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<Box<dyn TcpConnectionHandle>>, PortError>> {
        Box::pin(async move {
            let (stream, peer) = await_io(
                self.listener.accept(),
                deadline,
                cancellation,
                "system_socket.tcp_accept",
            )
            .await?;
            let config = tls_server_config(&material)?;
            let stream = await_io(
                TlsAcceptor::from(Arc::new(config)).accept(stream),
                deadline,
                cancellation,
                "system_socket.tls_handshake",
            )
            .await?;
            Ok(Some(Box::new(TokioTcpConnection {
                stream: Some(Box::new(stream)),
                peer,
            }) as Box<dyn TcpConnectionHandle>))
        })
    }
}

impl TcpConnectionHandle for TokioTcpConnection {
    fn peer_addr(&self) -> Result<SocketAddr, PortError> {
        Ok(self.peer)
    }

    fn start_tls<'a>(
        &'a mut self,
        material: Arc<TlsServerMaterial>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let stream = self.stream.take().ok_or_else(|| {
                PortError::new(PortErrorClass::Internal, "system_socket.tls_handshake")
            })?;
            let config = tls_server_config(&material)?;
            match await_io(
                TlsAcceptor::from(Arc::new(config)).accept(stream),
                deadline,
                cancellation,
                "system_socket.tls_handshake",
            )
            .await
            {
                Ok(stream) => {
                    self.stream = Some(Box::new(stream));
                    Ok(())
                }
                Err(error) => Err(error),
            }
        })
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
                let stream = self.stream.as_mut().ok_or_else(|| {
                    PortError::new(PortErrorClass::Internal, "system_socket.tcp_read_exact")
                })?;
                let count = await_io(
                    stream.read(&mut buffer[offset..]),
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
            let stream = self.stream.as_mut().ok_or_else(|| {
                PortError::new(PortErrorClass::Internal, "system_socket.tcp_read_chunk")
            })?;
            let count = await_io(
                stream.read(&mut buffer),
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
            let stream = self.stream.as_mut().ok_or_else(|| {
                PortError::new(PortErrorClass::Internal, "system_socket.tcp_write")
            })?;
            await_io(
                stream.write_all(&payload),
                deadline,
                cancellation,
                "system_socket.tcp_write",
            )
            .await
        })
    }

    fn shutdown(&mut self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let stream = self.stream.as_mut().ok_or_else(|| {
                PortError::new(PortErrorClass::Internal, "system_socket.tcp_shutdown")
            })?;
            stream
                .shutdown()
                .await
                .map_err(|error| map_io(error, "system_socket.tcp_shutdown"))
        })
    }
}

fn tls_server_config(material: &TlsServerMaterial) -> Result<ServerConfig, PortError> {
    if material.certificate_chain.is_empty() || material.private_key.is_empty() {
        return Err(PortError::new(
            PortErrorClass::InvalidInput,
            "system_socket.tls_config",
        ));
    }
    let certificates = material
        .certificate_chain
        .iter()
        .cloned()
        .map(CertificateDer::from)
        .collect::<Vec<_>>();
    let private_key = PrivateKeyDer::try_from(material.private_key.clone())
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "system_socket.tls_config"))?;
    let provider = rustls::crypto::ring::default_provider();
    ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "system_socket.tls_config"))?
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "system_socket.tls_config"))
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
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    use crate::dns::{Cancellation, Deadline};
    use crate::ports::effects::{
        ActivatedSocketHandle, SocketFactory, SocketKind, TcpReadChunkResult, TlsServerMaterial,
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

    #[tokio::test]
    async fn tcp_tls_acceptor_completes_handshake_and_preserves_peer() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let certificate_der = certified.cert.der().to_vec();
        let material = Arc::new(TlsServerMaterial {
            certificate_chain: vec![certificate_der.clone()],
            private_key: certified.signing_key.serialize_der(),
        });
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
        let address = listener.local_addr().unwrap();
        let server_cancellation = Cancellation::new();
        let server_listener = Arc::clone(&listener);
        let server_material = Arc::clone(&material);
        let server = tokio::spawn(async move {
            let mut connection = server_listener
                .accept_with_tls(
                    server_material,
                    Deadline::new(Instant::now() + Duration::from_secs(2)),
                    &server_cancellation,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                connection.peer_addr().unwrap().ip(),
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            );
            connection
                .read_chunk(
                    5,
                    Deadline::new(Instant::now() + Duration::from_secs(2)),
                    &server_cancellation,
                )
                .await
                .unwrap()
        });

        let client = TcpStream::connect(address).await.unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(certificate_der)).unwrap();
        let client_config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));
        let server_name = ServerName::try_from("localhost").unwrap();
        let mut client = connector.connect(server_name, client).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        assert_eq!(
            server.await.unwrap(),
            TcpReadChunkResult::Data(b"hello".to_vec())
        );
    }

    #[tokio::test]
    async fn tcp_tls_upgrade_observes_deadline_and_cancellation() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let material = Arc::new(TlsServerMaterial {
            certificate_chain: vec![certified.cert.der().to_vec()],
            private_key: certified.signing_key.serialize_der(),
        });
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
        let address = listener.local_addr().unwrap();

        let _silent_client = TcpStream::connect(address).await.unwrap();
        let mut connection = listener
            .accept(
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap()
            .unwrap();
        let timeout = connection
            .start_tls(
                Arc::clone(&material),
                Deadline::new(Instant::now() + Duration::from_millis(20)),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            timeout.class(),
            crate::ports::PortErrorClass::Timeout
        ));
        assert_eq!(timeout.operation(), "system_socket.tls_handshake");

        let _second_silent_client = TcpStream::connect(address).await.unwrap();
        let mut connection = listener
            .accept(
                Deadline::new(Instant::now() + Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap()
            .unwrap();
        let handshake = connection.start_tls(
            material,
            Deadline::new(Instant::now() + Duration::from_secs(1)),
            &cancellation,
        );
        cancellation.cancel(crate::dns::CancelReason::Shutdown);
        let cancelled = handshake.await.unwrap_err();
        assert!(matches!(
            cancelled.class(),
            crate::ports::PortErrorClass::Cancelled(crate::dns::CancelReason::Shutdown)
        ));
        assert_eq!(cancelled.operation(), "system_socket.tls_handshake");
    }
}
