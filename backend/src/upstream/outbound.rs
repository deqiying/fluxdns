//! Outbound proxy profile 与目标解析规划。
//!
//! 本模块只在显式 prepare/adapter 边界读取 SecretRef，并把 socks5/socks5h
//! 语义编译为不含原始 URL 的 profile；实际 SOCKS 握手和 socket dial 由后续
//! outbound adapter 完成。

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use thiserror::Error;
use url::Url;

use crate::config::resolve::{
    ConfigId, ProxyScheme, ResolvedOutbound, ResolvedSecretValue, SecretResolveError,
};

/// 已解析且可交给 outbound adapter 的代理 profile。
#[derive(Clone, Eq, PartialEq)]
pub struct OutboundProfile {
    id: ConfigId,
    scheme: ProxyScheme,
    proxy_host: Arc<str>,
    proxy_port: u16,
    proxy_url: ResolvedSecretValue,
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
        Ok(Self {
            id: outbound.id.clone(),
            scheme,
            proxy_host: Arc::from(host),
            proxy_port: port,
            proxy_url,
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
        let Ok(text) = std::str::from_utf8(self.proxy_url.expose()) else {
            return false;
        };
        Url::parse(text)
            .map(|url| !url.username().is_empty() || url.password().is_some())
            .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::resolve::{ConfigId, ResolvedOutbound, ResolvedSecretRef};

    use super::{NameResolution, OutboundProfile, OutboundTargetError};

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
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("fluxdns-outbound-{suffix}"));
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
}
