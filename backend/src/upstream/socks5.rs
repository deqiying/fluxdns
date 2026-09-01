//! SOCKS5/SOCKS5H 协议 codec。
//!
//! 该模块只负责受边界约束的握手帧构造与解析，不建立连接、不读取
//! SecretRef，也不把 socket 类型带入 upstream 核心。

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use thiserror::Error;

use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::{OutboundStream, TcpReadResult};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::{NameResolution, OutboundTarget};

pub const SOCKS5_VERSION: u8 = 0x05;
pub const USERPASS_VERSION: u8 = 0x01;
pub const NO_AUTHENTICATION: u8 = 0x00;
pub const USERNAME_PASSWORD: u8 = 0x02;
pub const NO_ACCEPTABLE_METHODS: u8 = 0xff;
pub const CONNECT_COMMAND: u8 = 0x01;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Socks5AuthMethod {
    NoAuthentication,
    UsernamePassword,
}

/// 构造 SOCKS5 method negotiation request。
pub fn encode_method_request(has_credentials: bool) -> Vec<u8> {
    if has_credentials {
        vec![SOCKS5_VERSION, 2, NO_AUTHENTICATION, USERNAME_PASSWORD]
    } else {
        vec![SOCKS5_VERSION, 1, NO_AUTHENTICATION]
    }
}

pub fn parse_method_response(frame: &[u8]) -> Result<Socks5AuthMethod, Socks5ProtocolError> {
    if frame.len() < 2 {
        return Err(Socks5ProtocolError::Truncated);
    }
    if frame.len() > 2 {
        return Err(Socks5ProtocolError::TrailingBytes);
    }
    if frame[0] != SOCKS5_VERSION {
        return Err(Socks5ProtocolError::InvalidVersion {
            expected: SOCKS5_VERSION,
            actual: frame[0],
        });
    }
    match frame[1] {
        NO_AUTHENTICATION => Ok(Socks5AuthMethod::NoAuthentication),
        USERNAME_PASSWORD => Ok(Socks5AuthMethod::UsernamePassword),
        NO_ACCEPTABLE_METHODS => Err(Socks5ProtocolError::NoAcceptableMethods),
        _ => Err(Socks5ProtocolError::UnsupportedMethod),
    }
}

pub fn encode_userpass_request(
    username: &[u8],
    password: &[u8],
) -> Result<Vec<u8>, Socks5ProtocolError> {
    validate_credential(username)?;
    validate_credential(password)?;
    let mut frame = Vec::with_capacity(3 + username.len() + password.len());
    frame.push(USERPASS_VERSION);
    frame.push(username.len() as u8);
    frame.extend_from_slice(username);
    frame.push(password.len() as u8);
    frame.extend_from_slice(password);
    Ok(frame)
}

pub fn parse_userpass_response(frame: &[u8]) -> Result<(), Socks5ProtocolError> {
    if frame.len() < 2 {
        return Err(Socks5ProtocolError::Truncated);
    }
    if frame.len() > 2 {
        return Err(Socks5ProtocolError::TrailingBytes);
    }
    if frame[0] != USERPASS_VERSION {
        return Err(Socks5ProtocolError::InvalidVersion {
            expected: USERPASS_VERSION,
            actual: frame[0],
        });
    }
    if frame[1] != 0 {
        return Err(Socks5ProtocolError::AuthenticationRejected);
    }
    Ok(())
}

#[derive(Clone, Eq, PartialEq)]
pub enum Socks5Address {
    Ip(IpAddr),
    Domain(Arc<[u8]>),
}

impl fmt::Debug for Socks5Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(address) => formatter.debug_tuple("Ip").field(address).finish(),
            Self::Domain(domain) => formatter
                .debug_struct("Domain")
                .field("byte_len", &domain.len())
                .finish(),
        }
    }
}

impl Socks5Address {
    pub fn ip(address: IpAddr) -> Self {
        Self::Ip(address)
    }

    fn domain(bytes: &[u8]) -> Result<Self, Socks5ProtocolError> {
        if bytes.is_empty() || bytes.len() > u8::MAX as usize {
            return Err(Socks5ProtocolError::InvalidDomain);
        }
        Ok(Self::Domain(Arc::from(bytes.to_vec().into_boxed_slice())))
    }

    pub fn as_ip(&self) -> Option<IpAddr> {
        match self {
            Self::Ip(address) => Some(*address),
            Self::Domain(_) => None,
        }
    }

    pub fn as_domain_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Ip(_) => None,
            Self::Domain(domain) => Some(domain),
        }
    }
}

/// 将 outbound target 编译为 SOCKS CONNECT 使用的地址。
pub fn address_for_target(
    target: &OutboundTarget,
    resolved_ip: Option<IpAddr>,
) -> Result<Socks5Address, Socks5TargetError> {
    match target.name_resolution() {
        NameResolution::Remote => {
            let bytes = target.host().as_bytes();
            if bytes.is_empty() || bytes.len() > u8::MAX as usize {
                return Err(Socks5TargetError::InvalidDomain);
            }
            Ok(Socks5Address::Domain(Arc::from(
                bytes.to_vec().into_boxed_slice(),
            )))
        }
        NameResolution::Local | NameResolution::Bypass => target
            .connect_ip()
            .or(resolved_ip)
            .map(Socks5Address::Ip)
            .ok_or(Socks5TargetError::ResolvedAddressRequired),
    }
}

pub fn encode_connect_request(
    address: &Socks5Address,
    port: u16,
) -> Result<Vec<u8>, Socks5ProtocolError> {
    if port == 0 {
        return Err(Socks5ProtocolError::InvalidPort);
    }
    let mut frame = Vec::with_capacity(4 + 16 + 2);
    frame.extend_from_slice(&[SOCKS5_VERSION, CONNECT_COMMAND, 0]);
    match address {
        Socks5Address::Ip(IpAddr::V4(address)) => {
            frame.push(0x01);
            frame.extend_from_slice(&address.octets());
        }
        Socks5Address::Ip(IpAddr::V6(address)) => {
            frame.push(0x04);
            frame.extend_from_slice(&address.octets());
        }
        Socks5Address::Domain(domain) => {
            if domain.is_empty() || domain.len() > u8::MAX as usize {
                return Err(Socks5ProtocolError::InvalidDomain);
            }
            frame.push(0x03);
            frame.push(domain.len() as u8);
            frame.extend_from_slice(domain);
        }
    }
    frame.extend_from_slice(&port.to_be_bytes());
    Ok(frame)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Socks5Reply {
    Succeeded,
    GeneralFailure,
    ConnectionNotAllowed,
    NetworkUnreachable,
    HostUnreachable,
    ConnectionRefused,
    TtlExpired,
    CommandNotSupported,
    AddressTypeNotSupported,
    Other(u8),
}

impl Socks5Reply {
    const fn from_wire(value: u8) -> Self {
        match value {
            0x00 => Self::Succeeded,
            0x01 => Self::GeneralFailure,
            0x02 => Self::ConnectionNotAllowed,
            0x03 => Self::NetworkUnreachable,
            0x04 => Self::HostUnreachable,
            0x05 => Self::ConnectionRefused,
            0x06 => Self::TtlExpired,
            0x07 => Self::CommandNotSupported,
            0x08 => Self::AddressTypeNotSupported,
            value => Self::Other(value),
        }
    }

    pub const fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Socks5ConnectResponse {
    reply: Socks5Reply,
    bound_address: Socks5Address,
    bound_port: u16,
}

impl Socks5ConnectResponse {
    pub fn reply(&self) -> Socks5Reply {
        self.reply
    }

    pub fn bound_address(&self) -> &Socks5Address {
        &self.bound_address
    }

    pub const fn bound_port(&self) -> u16 {
        self.bound_port
    }
}

pub fn parse_connect_response(frame: &[u8]) -> Result<Socks5ConnectResponse, Socks5ProtocolError> {
    if frame.len() < 4 {
        return Err(Socks5ProtocolError::Truncated);
    }
    if frame[0] != SOCKS5_VERSION {
        return Err(Socks5ProtocolError::InvalidVersion {
            expected: SOCKS5_VERSION,
            actual: frame[0],
        });
    }
    if frame[2] != 0 {
        return Err(Socks5ProtocolError::InvalidReservedField);
    }
    let (bound_address, address_end) = decode_address(frame, 3)?;
    let port_end = address_end
        .checked_add(2)
        .ok_or(Socks5ProtocolError::Truncated)?;
    if frame.len() < port_end {
        return Err(Socks5ProtocolError::Truncated);
    }
    if frame.len() > port_end {
        return Err(Socks5ProtocolError::TrailingBytes);
    }
    let bound_port = u16::from_be_bytes([frame[address_end], frame[address_end + 1]]);
    Ok(Socks5ConnectResponse {
        reply: Socks5Reply::from_wire(frame[1]),
        bound_address,
        bound_port,
    })
}

#[derive(Clone, Copy)]
pub struct Socks5Credentials<'a> {
    pub username: &'a [u8],
    pub password: &'a [u8],
}

impl fmt::Debug for Socks5Credentials<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Socks5Credentials")
            .field("username_len", &self.username.len())
            .field("password_len", &self.password.len())
            .finish()
    }
}

/// 在已连接的 outbound stream 上执行 SOCKS5 method、认证和 CONNECT。
pub fn perform_handshake<'a>(
    stream: &'a mut dyn OutboundStream,
    address: &'a Socks5Address,
    port: u16,
    credentials: Option<Socks5Credentials<'a>>,
    deadline: Deadline,
    cancellation: &'a Cancellation,
) -> PortFuture<'a, Result<(), Socks5HandshakeError>> {
    Box::pin(async move {
        if cancellation.is_cancelled() {
            return Err(Socks5HandshakeError::Transport(PortError::new(
                PortErrorClass::Cancelled(
                    cancellation
                        .reason()
                        .unwrap_or(CancelReason::UpstreamCancelled),
                ),
                "socks5.handshake",
            )));
        }

        write_frame(
            stream,
            encode_method_request(credentials.is_some()),
            deadline,
            cancellation,
        )
        .await?;
        let method = parse_method_response(
            &read_frame(stream, 2, deadline, cancellation, "socks5.method").await?,
        )
        .map_err(Socks5HandshakeError::Protocol)?;
        if matches!(method, Socks5AuthMethod::UsernamePassword) {
            let Some(credentials) = credentials else {
                return Err(Socks5HandshakeError::CredentialsRequired);
            };
            write_frame(
                stream,
                encode_userpass_request(credentials.username, credentials.password)
                    .map_err(Socks5HandshakeError::Protocol)?,
                deadline,
                cancellation,
            )
            .await?;
            parse_userpass_response(
                &read_frame(stream, 2, deadline, cancellation, "socks5.userpass").await?,
            )
            .map_err(Socks5HandshakeError::Protocol)?;
        }

        write_frame(
            stream,
            encode_connect_request(address, port).map_err(Socks5HandshakeError::Protocol)?,
            deadline,
            cancellation,
        )
        .await?;
        let response =
            parse_connect_response(&read_connect_response(stream, deadline, cancellation).await?)
                .map_err(Socks5HandshakeError::Protocol)?;
        if !response.reply().is_success() {
            return Err(Socks5HandshakeError::ProxyRejected(response.reply()));
        }
        Ok(())
    })
}

async fn write_frame(
    stream: &mut dyn OutboundStream,
    frame: Vec<u8>,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<(), Socks5HandshakeError> {
    stream
        .write_all(frame, deadline, cancellation)
        .await
        .map_err(Socks5HandshakeError::Transport)
}

async fn read_frame(
    stream: &mut dyn OutboundStream,
    length: usize,
    deadline: Deadline,
    cancellation: &Cancellation,
    operation: &'static str,
) -> Result<Vec<u8>, Socks5HandshakeError> {
    match stream
        .read_exact(length, deadline, cancellation)
        .await
        .map_err(Socks5HandshakeError::Transport)?
    {
        TcpReadResult::Complete(frame) if frame.len() == length => Ok(frame),
        TcpReadResult::Complete(_) => Err(Socks5HandshakeError::Transport(
            PortError::new(PortErrorClass::ProtocolViolation, operation)
                .with_safe_context("proxy returned an unexpected frame length"),
        )),
        TcpReadResult::CleanEof => Err(Socks5HandshakeError::Transport(
            PortError::new(PortErrorClass::ProtocolViolation, operation)
                .with_safe_context("proxy closed the stream during handshake"),
        )),
    }
}

async fn read_connect_response(
    stream: &mut dyn OutboundStream,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<Vec<u8>, Socks5HandshakeError> {
    let header = read_frame(stream, 4, deadline, cancellation, "socks5.connect").await?;
    let address_type = header[3];
    let mut frame = header;
    let remaining = match address_type {
        0x01 => 6,
        0x04 => 18,
        0x03 => {
            let length_frame =
                read_frame(stream, 1, deadline, cancellation, "socks5.connect").await?;
            let length = length_frame[0] as usize;
            if length == 0 {
                return Err(Socks5HandshakeError::Protocol(
                    Socks5ProtocolError::InvalidDomain,
                ));
            }
            frame.extend_from_slice(&length_frame);
            length + 2
        }
        _ => {
            return Err(Socks5HandshakeError::Protocol(
                Socks5ProtocolError::UnsupportedAddressType,
            ));
        }
    };
    frame.extend_from_slice(
        &read_frame(stream, remaining, deadline, cancellation, "socks5.connect").await?,
    );
    Ok(frame)
}

fn decode_address(
    frame: &[u8],
    offset: usize,
) -> Result<(Socks5Address, usize), Socks5ProtocolError> {
    let address_type = *frame.get(offset).ok_or(Socks5ProtocolError::Truncated)?;
    match address_type {
        0x01 => {
            let end = offset
                .checked_add(1 + 4)
                .ok_or(Socks5ProtocolError::Truncated)?;
            let bytes = frame
                .get(offset + 1..end)
                .ok_or(Socks5ProtocolError::Truncated)?;
            Ok((
                Socks5Address::Ip(IpAddr::from([bytes[0], bytes[1], bytes[2], bytes[3]])),
                end,
            ))
        }
        0x03 => {
            let length_offset = offset
                .checked_add(1)
                .ok_or(Socks5ProtocolError::Truncated)?;
            let length = *frame
                .get(length_offset)
                .ok_or(Socks5ProtocolError::Truncated)? as usize;
            let start = length_offset
                .checked_add(1)
                .ok_or(Socks5ProtocolError::Truncated)?;
            let end = start
                .checked_add(length)
                .ok_or(Socks5ProtocolError::Truncated)?;
            let bytes = frame
                .get(start..end)
                .ok_or(Socks5ProtocolError::Truncated)?;
            Ok((Socks5Address::domain(bytes)?, end))
        }
        0x04 => {
            let end = offset
                .checked_add(1 + 16)
                .ok_or(Socks5ProtocolError::Truncated)?;
            let bytes = frame
                .get(offset + 1..end)
                .ok_or(Socks5ProtocolError::Truncated)?;
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Ok((Socks5Address::Ip(IpAddr::from(octets)), end))
        }
        _ => Err(Socks5ProtocolError::UnsupportedAddressType),
    }
}

fn validate_credential(value: &[u8]) -> Result<(), Socks5ProtocolError> {
    if value.is_empty() || value.len() > u8::MAX as usize {
        return Err(Socks5ProtocolError::InvalidCredentialLength);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum Socks5TargetError {
    #[error("SOCKS5 target domain is invalid")]
    InvalidDomain,
    #[error("SOCKS5 target requires a resolved address")]
    ResolvedAddressRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum Socks5ProtocolError {
    #[error("SOCKS5 frame is truncated")]
    Truncated,
    #[error("SOCKS5 frame has trailing bytes")]
    TrailingBytes,
    #[error("SOCKS5 version is invalid")]
    InvalidVersion { expected: u8, actual: u8 },
    #[error("SOCKS5 method is unsupported")]
    UnsupportedMethod,
    #[error("SOCKS5 proxy does not accept any offered method")]
    NoAcceptableMethods,
    #[error("SOCKS5 username/password authentication was rejected")]
    AuthenticationRejected,
    #[error("SOCKS5 credential length is invalid")]
    InvalidCredentialLength,
    #[error("SOCKS5 target port is invalid")]
    InvalidPort,
    #[error("SOCKS5 target domain is invalid")]
    InvalidDomain,
    #[error("SOCKS5 reserved field is invalid")]
    InvalidReservedField,
    #[error("SOCKS5 address type is unsupported")]
    UnsupportedAddressType,
}

#[derive(Debug, Error)]
pub enum Socks5HandshakeError {
    #[error("SOCKS5 protocol error: {0}")]
    Protocol(#[source] Socks5ProtocolError),
    #[error("SOCKS5 stream operation failed: {0}")]
    Transport(#[source] PortError),
    #[error("SOCKS5 proxy selected username/password without credentials")]
    CredentialsRequired,
    #[error("SOCKS5 proxy rejected CONNECT: {0:?}")]
    ProxyRejected(Socks5Reply),
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use crate::config::resolve::{ConfigId, ResolvedOutbound, ResolvedSecretRef};
    use crate::dns::{Cancellation, Deadline};
    use crate::ports::effects::{OutboundStream, TcpReadResult};
    use crate::ports::{PortError, PortFuture};

    use super::{
        NameResolution, Socks5Address, Socks5AuthMethod, Socks5HandshakeError, Socks5ProtocolError,
        Socks5Reply, Socks5TargetError, address_for_target, encode_connect_request,
        encode_method_request, encode_userpass_request, parse_connect_response,
        parse_method_response, parse_userpass_response, perform_handshake,
    };
    use crate::upstream::OutboundProfile;

    struct FakeStream {
        reads: VecDeque<TcpReadResult>,
        writes: Vec<Vec<u8>>,
    }

    impl FakeStream {
        fn new(reads: impl IntoIterator<Item = TcpReadResult>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
                writes: Vec::new(),
            }
        }
    }

    impl OutboundStream for FakeStream {
        fn read_exact<'a>(
            &'a mut self,
            _length: usize,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<TcpReadResult, PortError>> {
            Box::pin(async move { Ok(self.reads.pop_front().unwrap_or(TcpReadResult::CleanEof)) })
        }

        fn write_all<'a>(
            &'a mut self,
            payload: Vec<u8>,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<(), PortError>> {
            Box::pin(async move {
                self.writes.push(payload);
                Ok(())
            })
        }

        fn shutdown(&mut self) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn profile(url: &str) -> OutboundProfile {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "fluxdns-socks5-test-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("proxy-url");
        std::fs::write(&path, url).unwrap();
        let outbound = ResolvedOutbound {
            id: ConfigId::new("proxy").unwrap(),
            kind: crate::config::model::OutboundType::Socks5,
            proxy_url: ResolvedSecretRef {
                env: None,
                file: Some(path),
            },
        };
        let profile = OutboundProfile::from_resolved(&outbound, 1024).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        profile
    }

    #[test]
    fn method_and_userpass_frames_are_bounded() {
        assert_eq!(encode_method_request(false), vec![5, 1, 0]);
        assert_eq!(encode_method_request(true), vec![5, 2, 0, 2]);
        assert_eq!(
            parse_method_response(&[5, 2]),
            Ok(Socks5AuthMethod::UsernamePassword)
        );
        assert_eq!(
            encode_userpass_request(b"user", b"pass").unwrap(),
            vec![1, 4, b'u', b's', b'e', b'r', 4, b'p', b'a', b's', b's']
        );
        assert_eq!(parse_userpass_response(&[1, 0]), Ok(()));
        assert_eq!(
            parse_userpass_response(&[1, 1]),
            Err(Socks5ProtocolError::AuthenticationRejected)
        );
    }

    #[test]
    fn method_and_credentials_reject_malformed_frames() {
        assert_eq!(
            parse_method_response(&[4, 0]),
            Err(Socks5ProtocolError::InvalidVersion {
                expected: 5,
                actual: 4,
            })
        );
        assert_eq!(
            parse_method_response(&[5, 255]),
            Err(Socks5ProtocolError::NoAcceptableMethods)
        );
        assert_eq!(
            encode_userpass_request(&[b'x'; 256], b"pass"),
            Err(Socks5ProtocolError::InvalidCredentialLength)
        );
        assert_eq!(
            parse_userpass_response(&[1]),
            Err(Socks5ProtocolError::Truncated)
        );
    }

    #[test]
    fn target_address_follows_local_remote_and_bypass_resolution() {
        let local = profile("socks5://proxy.example")
            .target(
                "dns.example",
                443,
                None,
                Some(ConfigId::new("bootstrap").unwrap()),
            )
            .unwrap();
        assert_eq!(local.name_resolution(), NameResolution::Local);
        assert_eq!(
            address_for_target(&local, Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)))),
            Ok(Socks5Address::Ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))))
        );
        assert_eq!(
            address_for_target(&local, None),
            Err(Socks5TargetError::ResolvedAddressRequired)
        );

        let remote = profile("socks5h://proxy.example")
            .target("dns.example", 443, None, None)
            .unwrap();
        assert_eq!(remote.name_resolution(), NameResolution::Remote);
        let address = address_for_target(&remote, None).unwrap();
        assert_eq!(address.as_domain_bytes(), Some(b"dns.example".as_slice()));

        let bypass = profile("socks5h://proxy.example")
            .target(
                "dns.example",
                443,
                Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
                None,
            )
            .unwrap();
        assert_eq!(bypass.name_resolution(), NameResolution::Bypass);
        assert_eq!(
            address_for_target(&bypass, None),
            Ok(Socks5Address::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)))
        );
    }

    #[test]
    fn connect_frames_encode_ip_and_domain_targets() {
        assert_eq!(
            encode_connect_request(
                &Socks5Address::Ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))),
                443
            )
            .unwrap(),
            vec![5, 1, 0, 1, 192, 0, 2, 10, 1, 187]
        );
        let domain = Socks5Address::Domain(std::sync::Arc::from(
            b"dns.example".to_vec().into_boxed_slice(),
        ));
        let frame = encode_connect_request(&domain, 53).unwrap();
        assert_eq!(&frame[..5], &[5, 1, 0, 3, 11]);
        assert_eq!(&frame[5..], b"dns.example\0\x35");
        assert_eq!(
            encode_connect_request(&Socks5Address::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)), 0),
            Err(Socks5ProtocolError::InvalidPort)
        );
    }

    #[test]
    fn connect_response_parses_bound_address_and_reply() {
        let response = parse_connect_response(&[5, 0, 0, 1, 192, 0, 2, 20, 0x1f, 0x90]).unwrap();
        assert_eq!(response.reply(), Socks5Reply::Succeeded);
        assert_eq!(
            response.bound_address().as_ip(),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20)))
        );
        assert_eq!(response.bound_port(), 8080);

        let response = parse_connect_response(&[
            5, 5, 0, 3, 11, b'd', b'n', b's', b'.', b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0, 53,
        ])
        .unwrap();
        assert_eq!(response.reply(), Socks5Reply::ConnectionRefused);
        assert_eq!(
            response.bound_address().as_domain_bytes(),
            Some(b"dns.example".as_slice())
        );
        assert_eq!(response.bound_port(), 53);
    }

    #[test]
    fn connect_response_rejects_malformed_frames() {
        assert_eq!(
            parse_connect_response(&[5, 0, 1, 0]),
            Err(Socks5ProtocolError::InvalidReservedField)
        );
        assert_eq!(
            parse_connect_response(&[5, 0, 0, 3, 4, b't', b'e']),
            Err(Socks5ProtocolError::Truncated)
        );
        assert_eq!(
            parse_connect_response(&[5, 0, 0, 9]),
            Err(Socks5ProtocolError::UnsupportedAddressType)
        );
        assert_eq!(
            parse_connect_response(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80, 0]),
            Err(Socks5ProtocolError::TrailingBytes)
        );
    }

    #[tokio::test]
    async fn handshake_runs_no_auth_and_domain_connect() {
        let target = profile("socks5h://proxy.example")
            .target("dns.example", 443, None, None)
            .unwrap();
        let address = address_for_target(&target, None).unwrap();
        let mut stream = FakeStream::new([
            TcpReadResult::Complete(vec![5, 0]),
            TcpReadResult::Complete(vec![5, 0, 0, 3]),
            TcpReadResult::Complete(vec![11]),
            TcpReadResult::Complete(vec![
                b'd', b'n', b's', b'.', b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x01, 0xbb,
            ]),
        ]);
        perform_handshake(
            &mut stream,
            &address,
            target.port(),
            None,
            Deadline::new(Instant::now() + std::time::Duration::from_secs(1)),
            &Cancellation::new(),
        )
        .await
        .unwrap();
        assert_eq!(stream.writes.len(), 2);
        assert_eq!(stream.writes[0], vec![5, 1, 0]);
        assert_eq!(&stream.writes[1][..5], &[5, 1, 0, 3, 11]);
    }

    #[tokio::test]
    async fn handshake_runs_username_password_and_rejects_proxy_reply() {
        let target = profile("socks5://proxy.example")
            .target("dns.example", 443, None, None)
            .unwrap();
        let address = address_for_target(&target, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))).unwrap();
        let mut stream = FakeStream::new([
            TcpReadResult::Complete(vec![5, 2]),
            TcpReadResult::Complete(vec![1, 0]),
            TcpReadResult::Complete(vec![5, 5, 0, 1]),
            TcpReadResult::Complete(vec![127, 0, 0, 1, 0, 80]),
        ]);
        let error = perform_handshake(
            &mut stream,
            &address,
            target.port(),
            Some(super::Socks5Credentials {
                username: b"user",
                password: b"pass",
            }),
            Deadline::new(Instant::now() + std::time::Duration::from_secs(1)),
            &Cancellation::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            Socks5HandshakeError::ProxyRejected(Socks5Reply::ConnectionRefused)
        ));
        assert_eq!(stream.writes[0], vec![5, 2, 0, 2]);
        assert_eq!(
            stream.writes[1],
            vec![1, 4, b'u', b's', b'e', b'r', 4, b'p', b'a', b's', b's']
        );
    }

    #[tokio::test]
    async fn handshake_maps_missing_credentials_and_clean_eof() {
        let target = profile("socks5h://proxy.example")
            .target("dns.example", 443, None, None)
            .unwrap();
        let address = address_for_target(&target, None).unwrap();
        let mut stream = FakeStream::new([TcpReadResult::Complete(vec![5, 2])]);
        let error = perform_handshake(
            &mut stream,
            &address,
            target.port(),
            None,
            Deadline::new(Instant::now() + std::time::Duration::from_secs(1)),
            &Cancellation::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Socks5HandshakeError::CredentialsRequired));

        let mut stream = FakeStream::new([TcpReadResult::CleanEof]);
        let error = perform_handshake(
            &mut stream,
            &address,
            target.port(),
            None,
            Deadline::new(Instant::now() + std::time::Duration::from_secs(1)),
            &Cancellation::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Socks5HandshakeError::Transport(_)));
    }
}
