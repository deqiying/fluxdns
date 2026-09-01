//! Outbound proxy profile 与目标解析规划。
//!
//! 本模块只在显式 prepare/adapter 边界读取 SecretRef，并把 socks5/socks5h
//! 语义编译为不含原始 URL 的 profile；实际 SOCKS 握手和 socket dial 由后续
//! outbound adapter 完成。

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use thiserror::Error;
use url::Url;

use crate::config::resolve::{
    ConfigId, ProxyScheme, ResolvedOutbound, ResolvedSecretValue, SecretResolveError,
};
use crate::dns::{Cancellation, Deadline};
use crate::ports::effects::{OutboundAddressResolver, OutboundDialer, OutboundStream};
use crate::ports::{PortError, PortFuture};

use super::socks5::{
    Socks5Credentials, Socks5HandshakeError, Socks5TargetError, address_for_target,
    perform_handshake,
};

/// 已解析且可交给 outbound adapter 的代理 profile。
#[derive(Clone, Eq, PartialEq)]
pub struct OutboundProfile {
    id: ConfigId,
    scheme: ProxyScheme,
    proxy_host: Arc<str>,
    proxy_port: u16,
    proxy_url: ResolvedSecretValue,
    credentials: Option<OutboundCredentials>,
}

impl fmt::Debug for OutboundProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutboundProfile")
            .field("id", &self.id)
            .field("scheme", &self.scheme)
            .field("has_proxy_host", &true)
            .field("proxy_port", &self.proxy_port)
            .field("has_credentials", &self.has_credentials())
            .finish()
    }
}

impl OutboundProfile {
    pub fn from_resolved(
        outbound: &ResolvedOutbound,
        max_secret_bytes: usize,
    ) -> Result<Self, OutboundProfileError> {
        let proxy_url = outbound
            .proxy_url
            .resolve_proxy_url(max_secret_bytes)
            .map_err(|source| OutboundProfileError::Secret {
                outbound: outbound.id.as_str().to_owned(),
                source,
            })?;
        let text =
            std::str::from_utf8(proxy_url.expose()).map_err(|_| OutboundProfileError::Secret {
                outbound: outbound.id.as_str().to_owned(),
                source: SecretResolveError::InvalidUtf8,
            })?;
        let url = Url::parse(text).map_err(|_| OutboundProfileError::InvalidProxyUrl {
            outbound: outbound.id.as_str().to_owned(),
        })?;
        let scheme = match url.scheme() {
            "socks5" => ProxyScheme::Socks5,
            "socks5h" => ProxyScheme::Socks5h,
            _ => {
                return Err(OutboundProfileError::InvalidProxyUrl {
                    outbound: outbound.id.as_str().to_owned(),
                });
            }
        };
        let Some(host) = url.host_str() else {
            return Err(OutboundProfileError::InvalidProxyUrl {
                outbound: outbound.id.as_str().to_owned(),
            });
        };
        let port = url.port().unwrap_or(1080);
        if port == 0
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(OutboundProfileError::InvalidProxyUrl {
                outbound: outbound.id.as_str().to_owned(),
            });
        }
        let credentials =
            parse_credentials(&url).map_err(|_| OutboundProfileError::InvalidCredentials {
                outbound: outbound.id.as_str().to_owned(),
            })?;
        Ok(Self {
            id: outbound.id.clone(),
            scheme,
            proxy_host: Arc::from(host),
            proxy_port: port,
            proxy_url,
            credentials,
        })
    }

    pub fn id(&self) -> &ConfigId {
        &self.id
    }

    pub const fn scheme(&self) -> ProxyScheme {
        self.scheme
    }

    pub fn proxy_host(&self) -> &str {
        &self.proxy_host
    }

    pub const fn proxy_port(&self) -> u16 {
        self.proxy_port
    }

    pub fn proxy_url(&self) -> &ResolvedSecretValue {
        &self.proxy_url
    }

    pub fn has_credentials(&self) -> bool {
        self.credentials.is_some()
    }

    pub fn credentials(&self) -> Option<&OutboundCredentials> {
        self.credentials.as_ref()
    }

    pub fn target(
        &self,
        host: &str,
        port: u16,
        connect_ip: Option<IpAddr>,
        bootstrap: Option<ConfigId>,
    ) -> Result<OutboundTarget, OutboundTargetError> {
        if host.is_empty() || host.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
            return Err(OutboundTargetError::InvalidHost);
        }
        if port == 0 {
            return Err(OutboundTargetError::InvalidPort);
        }
        if matches!(self.scheme, ProxyScheme::Socks5h) && bootstrap.is_some() {
            return Err(OutboundTargetError::BootstrapForbidden);
        }
        let name_resolution = if connect_ip.is_some() {
            NameResolution::Bypass
        } else if matches!(self.scheme, ProxyScheme::Socks5h) {
            NameResolution::Remote
        } else {
            NameResolution::Local
        };
        Ok(OutboundTarget {
            host: Arc::from(host),
            port,
            connect_ip,
            bootstrap,
            name_resolution,
        })
    }
}

/// outbound adapter 的目标规划，不执行 DNS 或 socket I/O。
#[derive(Clone, Eq, PartialEq)]
pub struct OutboundTarget {
    host: Arc<str>,
    port: u16,
    connect_ip: Option<IpAddr>,
    bootstrap: Option<ConfigId>,
    name_resolution: NameResolution,
}

impl fmt::Debug for OutboundTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutboundTarget")
            .field("has_host", &true)
            .field("port", &self.port)
            .field("has_connect_ip", &self.connect_ip.is_some())
            .field("has_bootstrap", &self.bootstrap.is_some())
            .field("name_resolution", &self.name_resolution)
            .finish()
    }
}

impl OutboundTarget {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    pub const fn connect_ip(&self) -> Option<IpAddr> {
        self.connect_ip
    }

    pub fn bootstrap(&self) -> Option<&ConfigId> {
        self.bootstrap.as_ref()
    }

    pub const fn name_resolution(&self) -> NameResolution {
        self.name_resolution
    }
}

/// 使用调用方提供的 proxy 地址和 dialer 建立 SOCKS5 outbound stream。
pub struct Socks5Connector<D> {
    dialer: Arc<D>,
}

impl<D> Clone for Socks5Connector<D> {
    fn clone(&self) -> Self {
        Self {
            dialer: Arc::clone(&self.dialer),
        }
    }
}

impl<D: OutboundDialer> Socks5Connector<D> {
    pub fn new(dialer: Arc<D>) -> Self {
        Self { dialer }
    }

    pub fn connect<'a>(
        &'a self,
        proxy_address: SocketAddr,
        target: &'a OutboundTarget,
        resolved_ip: Option<IpAddr>,
        credentials: Option<Socks5Credentials<'a>>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Box<dyn OutboundStream>, Socks5ConnectError>> {
        Box::pin(async move {
            let address =
                address_for_target(target, resolved_ip).map_err(Socks5ConnectError::Target)?;
            let mut stream = self
                .dialer
                .connect(proxy_address, deadline, cancellation)
                .await
                .map_err(Socks5ConnectError::Dial)?;
            perform_handshake(
                &mut *stream,
                &address,
                target.port(),
                credentials,
                deadline,
                cancellation,
            )
            .await
            .map_err(Socks5ConnectError::Handshake)?;
            Ok(stream)
        })
    }

    pub fn connect_profile<'a>(
        &'a self,
        profile: &'a OutboundProfile,
        proxy_address: SocketAddr,
        target: &'a OutboundTarget,
        resolved_ip: Option<IpAddr>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Box<dyn OutboundStream>, Socks5ConnectError>> {
        let credentials = profile.credentials().map(|credentials| Socks5Credentials {
            username: credentials.username(),
            password: credentials.password(),
        });
        self.connect(
            proxy_address,
            target,
            resolved_ip,
            credentials,
            deadline,
            cancellation,
        )
    }

    pub fn connect_profile_with_resolver<'a>(
        &'a self,
        profile: &'a OutboundProfile,
        resolver: &'a dyn OutboundAddressResolver,
        target: &'a OutboundTarget,
        resolved_ip: Option<IpAddr>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Box<dyn OutboundStream>, Socks5ConnectError>> {
        Box::pin(async move {
            let proxy_address = if let Ok(address) = profile.proxy_host().parse::<IpAddr>() {
                SocketAddr::new(address, profile.proxy_port())
            } else {
                let addresses = resolver
                    .resolve(
                        profile.proxy_host(),
                        profile.proxy_port(),
                        deadline,
                        cancellation,
                    )
                    .await
                    .map_err(Socks5ConnectError::ProxyResolve)?;
                addresses.into_iter().next().ok_or_else(|| {
                    Socks5ConnectError::ProxyResolve(PortError::new(
                        crate::ports::PortErrorClass::Unavailable,
                        "outbound.proxy_resolve",
                    ))
                })?
            };
            self.connect_profile(
                profile,
                proxy_address,
                target,
                resolved_ip,
                deadline,
                cancellation,
            )
            .await
        })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct OutboundCredentials {
    username: Arc<[u8]>,
    password: Arc<[u8]>,
}

impl fmt::Debug for OutboundCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutboundCredentials")
            .field("username_len", &self.username.len())
            .field("password_len", &self.password.len())
            .finish()
    }
}

impl OutboundCredentials {
    pub fn username(&self) -> &[u8] {
        &self.username
    }

    pub fn password(&self) -> &[u8] {
        &self.password
    }
}

fn parse_credentials(url: &Url) -> Result<Option<OutboundCredentials>, ()> {
    let username = url.username();
    let Some(password) = url.password() else {
        return if username.is_empty() {
            Ok(None)
        } else {
            Err(())
        };
    };
    let username = percent_decode_userinfo(username)?;
    let password = percent_decode_userinfo(password)?;
    if username.is_empty()
        || password.is_empty()
        || username.len() > u8::MAX as usize
        || password.len() > u8::MAX as usize
    {
        return Err(());
    }
    Ok(Some(OutboundCredentials {
        username: Arc::from(username.into_boxed_slice()),
        password: Arc::from(password.into_boxed_slice()),
    }))
}

fn percent_decode_userinfo(value: &str) -> Result<Vec<u8>, ()> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).copied().ok_or(())?;
            let low = bytes.get(index + 2).copied().ok_or(())?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(decoded)
}

fn hex_value(value: u8) -> Result<u8, ()> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameResolution {
    Local,
    Remote,
    Bypass,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum OutboundProfileError {
    #[error("outbound `{outbound}` secret resolution failed: {source}")]
    Secret {
        outbound: String,
        source: SecretResolveError,
    },
    #[error("outbound `{outbound}` proxy URL has an unsupported shape")]
    InvalidProxyUrl { outbound: String },
    #[error("outbound `{outbound}` proxy credentials are invalid")]
    InvalidCredentials { outbound: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum OutboundTargetError {
    #[error("outbound target host is invalid")]
    InvalidHost,
    #[error("outbound target port is invalid")]
    InvalidPort,
    #[error("socks5h outbound cannot use bootstrap")]
    BootstrapForbidden,
}

#[derive(Debug, Error)]
pub enum Socks5ConnectError {
    #[error("SOCKS5 target preparation failed: {0}")]
    Target(#[source] Socks5TargetError),
    #[error("SOCKS5 proxy dial failed: {0}")]
    Dial(#[source] PortError),
    #[error("SOCKS5 proxy address resolution failed: {0}")]
    ProxyResolve(#[source] PortError),
    #[error("SOCKS5 handshake failed: {0}")]
    Handshake(#[source] Socks5HandshakeError),
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::resolve::{ConfigId, ResolvedOutbound, ResolvedSecretRef};
    use crate::dns::{Cancellation, Deadline};
    use crate::ports::effects::OutboundAddressResolver;
    use crate::ports::{PortError, PortFuture};
    use crate::upstream::{OutboundProfileError, Socks5Connector, TokioOutboundDialer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{NameResolution, OutboundProfile, OutboundTargetError};

    struct FakeResolver {
        address: SocketAddr,
    }

    impl OutboundAddressResolver for FakeResolver {
        fn resolve<'a>(
            &'a self,
            _host: &'a str,
            _port: u16,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Vec<SocketAddr>, PortError>> {
            let address = self.address;
            Box::pin(async move { Ok(vec![address]) })
        }
    }

    fn outbound(path: std::path::PathBuf) -> ResolvedOutbound {
        ResolvedOutbound {
            id: ConfigId::new("proxy").unwrap(),
            kind: crate::config::model::OutboundType::Socks5,
            proxy_url: ResolvedSecretRef {
                env: None,
                file: Some(path),
            },
        }
    }

    fn secret_file(contents: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "fluxdns-outbound-{}-{suffix}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("proxy-url");
        fs::write(&path, contents).unwrap();
        (root, path)
    }

    #[test]
    fn profile_resolves_secret_url_and_redacts_credentials() {
        let (root, path) = secret_file(b"  socks5://user:password@proxy.example:1081\n");
        let profile = OutboundProfile::from_resolved(&outbound(path), 1024).unwrap();

        assert_eq!(profile.proxy_host(), "proxy.example");
        assert_eq!(profile.proxy_port(), 1081);
        assert!(profile.has_credentials());
        assert!(!format!("{profile:?}").contains("password"));
        assert_eq!(
            profile.proxy_url().expose(),
            b"socks5://user:password@proxy.example:1081"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_decodes_bounded_credentials_without_exposing_them() {
        let (root, path) = secret_file(b"socks5://us%65r:p%40ss@proxy.example");
        let profile = OutboundProfile::from_resolved(&outbound(path), 1024).unwrap();
        let credentials = profile.credentials().unwrap();
        assert_eq!(credentials.username(), b"user");
        assert_eq!(credentials.password(), b"p@ss");
        assert!(!format!("{credentials:?}").contains("p@ss"));
        fs::remove_dir_all(root).unwrap();

        let (root, path) = secret_file(b"socks5://user-only@proxy.example");
        assert!(matches!(
            OutboundProfile::from_resolved(&outbound(path), 1024),
            Err(OutboundProfileError::InvalidCredentials { .. })
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn target_keeps_bootstrap_for_local_socks5_resolution() {
        let (root, path) = secret_file(b"socks5://proxy.example");
        let profile = OutboundProfile::from_resolved(&outbound(path), 1024).unwrap();
        let target = profile
            .target(
                "dns.example",
                443,
                None,
                Some(ConfigId::new("bootstrap").unwrap()),
            )
            .unwrap();

        assert_eq!(target.name_resolution(), NameResolution::Local);
        assert_eq!(target.bootstrap().map(ConfigId::as_str), Some("bootstrap"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socks5h_without_connect_ip_uses_remote_name_resolution() {
        let (root, path) = secret_file(b"socks5h://proxy.example");
        let profile = OutboundProfile::from_resolved(&outbound(path), 1024).unwrap();
        let target = profile.target("dns.example", 443, None, None).unwrap();
        assert_eq!(target.name_resolution(), NameResolution::Remote);
        let explicit_ip = profile
            .target(
                "dns.example",
                443,
                Some("192.0.2.10".parse().unwrap()),
                None,
            )
            .unwrap();
        assert_eq!(explicit_ip.name_resolution(), NameResolution::Bypass);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socks5h_rejects_bootstrap_and_invalid_targets() {
        let (root, path) = secret_file(b"socks5h://proxy.example");
        let profile = OutboundProfile::from_resolved(&outbound(path), 1024).unwrap();
        assert_eq!(
            profile.target(
                "dns.example",
                443,
                None,
                Some(ConfigId::new("bootstrap").unwrap())
            ),
            Err(OutboundTargetError::BootstrapForbidden)
        );
        assert_eq!(
            profile.target("dns\nexample", 443, None, None),
            Err(OutboundTargetError::InvalidHost)
        );
        assert_eq!(
            profile.target("dns.example", 0, None, None),
            Err(OutboundTargetError::InvalidPort)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn tokio_socks5_connector_dials_loopback_proxy() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();

            let mut request = [0; 10];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [5, 1, 0, 1, 192, 0, 2, 10, 1, 187]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();
        });

        let (root, path) = secret_file(b"socks5://proxy.example");
        let profile = OutboundProfile::from_resolved(&outbound(path), 1024).unwrap();
        let target = profile
            .target(
                "dns.example",
                443,
                Some("192.0.2.10".parse().unwrap()),
                None,
            )
            .unwrap();
        let connector = Socks5Connector::new(Arc::new(TokioOutboundDialer::new()));
        let cancellation = Cancellation::new();
        let mut stream = connector
            .connect(
                proxy_address,
                &target,
                None,
                None,
                Deadline::new(std::time::Instant::now() + std::time::Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        server.await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn connect_profile_wires_username_password_credentials() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 4];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 2, 0, 2]);
            stream.write_all(&[5, 2]).await.unwrap();

            let mut credentials = [0; 11];
            stream.read_exact(&mut credentials).await.unwrap();
            assert_eq!(
                credentials,
                [1, 4, b'u', b's', b'e', b'r', 4, b'p', b'@', b's', b's']
            );
            stream.write_all(&[1, 0]).await.unwrap();

            let mut request = [0; 10];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [5, 1, 0, 1, 192, 0, 2, 10, 1, 187]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();
        });

        let (root, path) = secret_file(b"socks5://user:p%40ss@proxy.example");
        let profile = OutboundProfile::from_resolved(&outbound(path), 1024).unwrap();
        let target = profile
            .target(
                "dns.example",
                443,
                Some("192.0.2.10".parse().unwrap()),
                None,
            )
            .unwrap();
        let connector = Socks5Connector::new(Arc::new(TokioOutboundDialer::new()));
        let cancellation = Cancellation::new();
        let mut stream = connector
            .connect_profile(
                &profile,
                proxy_address,
                &target,
                None,
                Deadline::new(std::time::Instant::now() + std::time::Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        server.await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn connect_profile_resolves_proxy_hostname_before_dial() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();
            let mut request = [0; 10];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [5, 1, 0, 1, 192, 0, 2, 10, 1, 187]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();
        });

        let (root, path) = secret_file(b"socks5://proxy.test");
        let profile = OutboundProfile::from_resolved(&outbound(path), 1024).unwrap();
        let target = profile
            .target(
                "dns.example",
                443,
                Some("192.0.2.10".parse().unwrap()),
                None,
            )
            .unwrap();
        let resolver = FakeResolver {
            address: proxy_address,
        };
        let connector = Socks5Connector::new(Arc::new(TokioOutboundDialer::new()));
        let cancellation = Cancellation::new();
        let mut stream = connector
            .connect_profile_with_resolver(
                &profile,
                &resolver,
                &target,
                None,
                Deadline::new(std::time::Instant::now() + std::time::Duration::from_secs(1)),
                &cancellation,
            )
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        server.await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
