//! Normalization and immutable resolved configuration construction.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ipnet::IpNet;
use url::Url;

use super::migrate::deterministic_hash;
use super::model::{
    CacheOverrideDto, ClientDto, ClientIpDto, ClientIpSource, ConfigDto, DatabaseType, EcsDto,
    EcsMode, ForwardedDisposition, ForwardedHeader, GlobalCacheDto, HostsResourceDto, ListenerDto,
    LogLevelDto, OptimisticDto, OutboundDto, RuleSetDto, StrategyDto, TlsMode, UpstreamDto,
};
use super::validate::{
    BindPlan, ConfigError, ConfigErrorKind, ConfigErrorReport, DohBindingRef, build_bind_plan,
    validate_config,
};

struct SafeUrl<'a>(&'a Url);

impl fmt::Debug for SafeUrl<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut url = self.0.clone();
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_path("");
        url.set_query(None);
        url.set_fragment(None);
        formatter.debug_tuple("Url").field(&url.as_str()).finish()
    }
}

/// A validated identifier issued by the configuration boundary.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConfigId(Arc<str>);

impl ConfigId {
    pub fn new(value: impl Into<Arc<str>>) -> Result<Self, ConfigError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "name",
                "identifier must contain 1..=128 characters",
            ));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!'))
        {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "name",
                "identifier contains an unsupported character",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Normalized work-directory paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedWork {
    pub path: PathBuf,
    pub rules_path: PathBuf,
    pub snapshot_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDatabase {
    pub kind: DatabaseType,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedLogs {
    pub enable: bool,
    pub level: LogLevelDto,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedWebUi {
    pub enable: bool,
    pub address: std::net::IpAddr,
    pub port: u16,
    pub users: Vec<ResolvedWebUiUser>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedWebUiUser {
    pub name: String,
    password_hash: String,
}

impl fmt::Debug for ResolvedWebUiUser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedWebUiUser")
            .field("name", &self.name)
            .field("password_hash", &"[REDACTED]")
            .finish()
    }
}

impl ResolvedWebUiUser {
    pub fn verify_hash_format(&self) -> bool {
        let value = &self.password_hash;
        (value.starts_with("$2a$") || value.starts_with("$2b$") || value.starts_with("$2y$"))
            && value.len() == 60
            || (value.starts_with("$argon2id$") && value.len() >= 20)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDns {
    pub cache: ResolvedGlobalCache,
    pub ttl_override: ResolvedTtlOverride,
    pub edns_client_subnet: ResolvedEcs,
    pub resolve_log: ResolvedResolveLog,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedGlobalCache {
    pub enabled: bool,
    pub memory_max_size_bytes: u64,
    pub failure_ttl: Duration,
    pub optimistic: ResolvedOptimistic,
    pub persistence_path: PathBuf,
    pub persistence_max_size_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedOptimistic {
    pub enabled: bool,
    pub answer_ttl: Duration,
    pub max_age: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCacheOverride {
    pub enabled: bool,
    pub optimistic: Option<ResolvedOptimistic>,
    pub source: ValueSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTtlOverride {
    pub enabled: bool,
    pub min: Option<Duration>,
    pub max: Option<Duration>,
    pub source: ValueSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedEcs {
    pub mode: EcsMode,
    pub custom_ip: Option<IpNet>,
    pub source: ValueSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueSource {
    Default,
    Global,
    Upstream,
    Client,
    Strategy,
    Rule,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedResolveLog {
    pub enable: bool,
    pub eviction_threshold_records: u64,
    pub max_records: u64,
    pub max_record_age: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedListener {
    Udp {
        id: ConfigId,
        addresses: Vec<std::net::IpAddr>,
        port: u16,
        strategy: ConfigId,
        hosts: Option<ConfigId>,
    },
    Tcp {
        id: ConfigId,
        addresses: Vec<std::net::IpAddr>,
        port: u16,
        strategy: ConfigId,
        hosts: Option<ConfigId>,
    },
    Doh {
        id: ConfigId,
        routes: Vec<ResolvedDohRoute>,
        endpoints: Vec<ResolvedDohEndpoint>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDohRoute {
    pub path: String,
    pub strategy: ConfigId,
    pub edns_client_subnet: Option<ResolvedEcs>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDohEndpoint {
    pub id: ConfigId,
    pub binding: DohBindingRef,
    pub addresses: Vec<std::net::IpAddr>,
    pub port: u16,
    pub tls_mode: TlsMode,
    pub certificate_file: Option<PathBuf>,
    pub private_key_file: Option<PathBuf>,
    pub client_ip: ResolvedClientIp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedClientIp {
    pub source: ClientIpSource,
    pub header: Option<ForwardedHeader>,
    pub trusted_proxies: Option<Vec<IpNet>>,
    pub on_missing: Option<ForwardedDisposition>,
    pub on_invalid: Option<ForwardedDisposition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedUpstreamMember {
    pub name: ConfigId,
    pub weight: u32,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ResolvedUpstream {
    Hosts {
        id: ConfigId,
        format: String,
        hosts: String,
    },
    Doh {
        id: ConfigId,
        address: Url,
        bootstrap: Option<ConfigId>,
        connect_ip: Option<std::net::IpAddr>,
        proxy: Option<ConfigId>,
        edns_client_subnet: Option<ResolvedEcs>,
    },
    Group {
        id: ConfigId,
        upstreams: Vec<ResolvedUpstreamMember>,
        upstream_mode: super::model::UpstreamMode,
        timeout: Duration,
        fallbacks: Vec<ResolvedUpstreamMember>,
        fallback_upstream_mode: Option<super::model::UpstreamMode>,
        fallback_timeout: Option<Duration>,
    },
}

impl fmt::Debug for ResolvedUpstream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hosts { id, format, hosts } => formatter
                .debug_struct("ResolvedUpstream::Hosts")
                .field("id", id)
                .field("format", format)
                .field("hosts_len", &hosts.len())
                .finish(),
            Self::Doh {
                id,
                address,
                bootstrap,
                connect_ip,
                proxy,
                edns_client_subnet,
            } => formatter
                .debug_struct("ResolvedUpstream::Doh")
                .field("id", id)
                .field("address", &SafeUrl(address))
                .field("bootstrap", bootstrap)
                .field("connect_ip", connect_ip)
                .field("proxy", proxy)
                .field("edns_client_subnet", edns_client_subnet)
                .finish(),
            Self::Group {
                id,
                upstreams,
                upstream_mode,
                timeout,
                fallbacks,
                fallback_upstream_mode,
                fallback_timeout,
            } => formatter
                .debug_struct("ResolvedUpstream::Group")
                .field("id", id)
                .field("upstreams", upstreams)
                .field("upstream_mode", upstream_mode)
                .field("timeout", timeout)
                .field("fallbacks", fallbacks)
                .field("fallback_upstream_mode", fallback_upstream_mode)
                .field("fallback_timeout", fallback_timeout)
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedStrategy {
    pub id: ConfigId,
    pub rules: Vec<ResolvedStrategyRule>,
    pub default_upstream: ConfigId,
    pub cache: Option<ResolvedCacheOverride>,
    pub ttl_override: ResolvedTtlOverride,
    pub edns_client_subnet: ResolvedEcs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedStrategyRule {
    pub rule_set: Option<ResolvedRuleSetRef>,
    pub hosts: Option<ConfigId>,
    pub upstream: Option<ConfigId>,
    pub edns_client_subnet: ResolvedEcs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRuleSetRef {
    pub resource: ConfigId,
    pub selector: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedHostsResource {
    Const {
        id: ConfigId,
        format: super::model::HostsFormat,
        hosts: String,
    },
    File {
        id: ConfigId,
        format: super::model::HostsFormat,
        path: PathBuf,
        auto_update: bool,
        update_interval: Option<Duration>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedOutbound {
    pub id: ConfigId,
    pub kind: super::model::OutboundType,
    pub proxy_url: ResolvedSecretRef,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedSecretRef {
    pub env: Option<String>,
    pub file: Option<PathBuf>,
}

impl fmt::Debug for ResolvedSecretRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretRef([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretSourceKind {
    Environment,
    File,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedSecretValue(Box<[u8]>);

impl ResolvedSecretValue {
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for ResolvedSecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

impl fmt::Display for ResolvedSecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretResolveError {
    InvalidReference,
    Missing { source: SecretSourceKind },
    Empty { source: SecretSourceKind },
    TooLarge { limit: usize },
    InvalidUtf8,
    Io,
    InvalidProxyUrl,
    UnsupportedProxyScheme,
}

impl fmt::Display for SecretResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReference => formatter.write_str("secret reference is invalid"),
            Self::Missing { source } => write!(formatter, "secret source is missing ({source:?})"),
            Self::Empty { source } => write!(formatter, "secret source is empty ({source:?})"),
            Self::TooLarge { limit } => write!(formatter, "secret exceeds the {limit} byte limit"),
            Self::InvalidUtf8 => formatter.write_str("secret is not valid UTF-8"),
            Self::Io => formatter.write_str("secret source could not be read"),
            Self::InvalidProxyUrl => formatter.write_str("proxy URL is invalid"),
            Self::UnsupportedProxyScheme => formatter.write_str("proxy URL scheme is unsupported"),
        }
    }
}

impl std::error::Error for SecretResolveError {}

impl ResolvedSecretRef {
    pub fn source_kind(&self) -> Option<SecretSourceKind> {
        match (self.env.is_some(), self.file.is_some()) {
            (true, false) => Some(SecretSourceKind::Environment),
            (false, true) => Some(SecretSourceKind::File),
            _ => None,
        }
    }

    /// Resolve a secret only when a caller explicitly requests the side effect.
    /// The normal YAML load path keeps the reference, never the value.
    pub fn resolve(&self, max_bytes: usize) -> Result<ResolvedSecretValue, SecretResolveError> {
        let (source, bytes) = match (&self.env, &self.file) {
            (Some(name), None) if !name.trim().is_empty() => {
                let value = std::env::var_os(name).ok_or(SecretResolveError::Missing {
                    source: SecretSourceKind::Environment,
                })?;
                let bytes = value
                    .into_string()
                    .map_err(|_| SecretResolveError::InvalidUtf8)?
                    .into_bytes();
                (SecretSourceKind::Environment, bytes)
            }
            (None, Some(path)) if !path.as_os_str().is_empty() => {
                let file = File::open(path).map_err(|_| SecretResolveError::Io)?;
                let mut bytes = Vec::new();
                file.take((max_bytes as u64).saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(|_| SecretResolveError::Io)?;
                (SecretSourceKind::File, bytes)
            }
            _ => return Err(SecretResolveError::InvalidReference),
        };
        if bytes.len() > max_bytes {
            return Err(SecretResolveError::TooLarge { limit: max_bytes });
        }
        if bytes.is_empty() || bytes.iter().all(u8::is_ascii_whitespace) {
            return Err(SecretResolveError::Empty { source });
        }
        Ok(ResolvedSecretValue(bytes.into_boxed_slice()))
    }

    pub fn resolve_proxy_url(
        &self,
        max_bytes: usize,
    ) -> Result<ResolvedSecretValue, SecretResolveError> {
        let value = self.resolve(max_bytes)?;
        let text = std::str::from_utf8(value.expose())
            .map_err(|_| SecretResolveError::InvalidUtf8)?
            .trim();
        let url = Url::parse(text).map_err(|_| SecretResolveError::InvalidProxyUrl)?;
        if !matches!(url.scheme(), "socks5" | "socks5h") {
            return Err(SecretResolveError::UnsupportedProxyScheme);
        }
        if url.host_str().is_none() {
            return Err(SecretResolveError::InvalidProxyUrl);
        }
        if text.len() == value.expose().len() {
            Ok(value)
        } else {
            Ok(ResolvedSecretValue(
                text.as_bytes().to_vec().into_boxed_slice(),
            ))
        }
    }

    pub fn proxy_scheme(&self, max_bytes: usize) -> Result<ProxyScheme, SecretResolveError> {
        let value = self.resolve_proxy_url(max_bytes)?;
        let text = std::str::from_utf8(value.expose())
            .map_err(|_| SecretResolveError::InvalidUtf8)?
            .trim();
        let url = Url::parse(text).map_err(|_| SecretResolveError::InvalidProxyUrl)?;
        match url.scheme() {
            "socks5" => Ok(ProxyScheme::Socks5),
            "socks5h" => Ok(ProxyScheme::Socks5h),
            _ => Err(SecretResolveError::UnsupportedProxyScheme),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyScheme {
    Socks5,
    Socks5h,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ResolvedRuleSet {
    Const {
        id: ConfigId,
        format: super::model::RuleSetFormat,
        rule: String,
    },
    File {
        id: ConfigId,
        format: super::model::RuleSetFormat,
        path: PathBuf,
        auto_update: bool,
        update_interval: Option<Duration>,
    },
    Remote {
        id: ConfigId,
        format: super::model::RuleSetFormat,
        url: Url,
        proxy: Option<ConfigId>,
        auto_update: bool,
        update_interval: Option<Duration>,
    },
}

impl fmt::Debug for ResolvedRuleSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Const { id, format, rule } => formatter
                .debug_struct("ResolvedRuleSet::Const")
                .field("id", id)
                .field("format", format)
                .field("rule_len", &rule.len())
                .finish(),
            Self::File {
                id,
                format,
                path,
                auto_update,
                update_interval,
            } => formatter
                .debug_struct("ResolvedRuleSet::File")
                .field("id", id)
                .field("format", format)
                .field("path", path)
                .field("auto_update", auto_update)
                .field("update_interval", update_interval)
                .finish(),
            Self::Remote {
                id,
                format,
                url,
                proxy,
                auto_update,
                update_interval,
            } => formatter
                .debug_struct("ResolvedRuleSet::Remote")
                .field("id", id)
                .field("format", format)
                .field("url", &SafeUrl(url))
                .field("proxy", proxy)
                .field("auto_update", auto_update)
                .field("update_interval", update_interval)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedClient {
    pub id: ConfigId,
    pub ids: Vec<String>,
    pub ips: Vec<IpNet>,
    pub strategy: Option<ConfigId>,
    pub cache: Option<ResolvedCacheOverride>,
    pub ttl_override: ResolvedTtlOverride,
    pub edns_client_subnet: ResolvedEcs,
}

impl fmt::Debug for ResolvedClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedClient")
            .field("id", &self.id)
            .field("id_count", &self.ids.len())
            .field("ip_count", &self.ips.len())
            .field("strategy", &self.strategy)
            .field("cache", &self.cache)
            .field("ttl_override", &self.ttl_override)
            .field("edns_client_subnet", &self.edns_client_subnet)
            .finish()
    }
}

/// Immutable configuration consumed by later runtime phases.
pub struct ResolvedConfig {
    pub version: u32,
    pub work: ResolvedWork,
    pub database: ResolvedDatabase,
    pub logs: ResolvedLogs,
    pub webui: ResolvedWebUi,
    pub dns: ResolvedDns,
    pub listeners: Vec<ResolvedListener>,
    pub upstreams: Vec<ResolvedUpstream>,
    pub strategies: Vec<ResolvedStrategy>,
    pub hosts: Vec<ResolvedHostsResource>,
    pub outbounds: Vec<ResolvedOutbound>,
    pub rule_sets: Vec<ResolvedRuleSet>,
    pub clients: Vec<ResolvedClient>,
    pub bind_plan: BindPlan,
    pub input_hash: String,
    pub normalized_hash: String,
}

impl fmt::Debug for ResolvedConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedConfig")
            .field("version", &self.version)
            .field("work_path", &self.work.path)
            .field("listener_count", &self.listeners.len())
            .field("upstream_count", &self.upstreams.len())
            .field("strategy_count", &self.strategies.len())
            .field("host_count", &self.hosts.len())
            .field("rule_set_count", &self.rule_sets.len())
            .field("client_count", &self.clients.len())
            .field("input_hash", &self.input_hash)
            .field("normalized_hash", &self.normalized_hash)
            .finish()
    }
}

impl ResolvedConfig {
    pub fn redacted_view(&self) -> RedactedConfigView {
        RedactedConfigView {
            version: self.version,
            work_path: self.work.path.clone(),
            listener_count: self.listeners.len(),
            upstream_count: self.upstreams.len(),
            strategy_count: self.strategies.len(),
            input_hash: self.input_hash.clone(),
            normalized_hash: self.normalized_hash.clone(),
        }
    }

    /// Resolve and validate configured proxy references on an explicit prepare boundary.
    /// Loading a YAML file itself never reads the environment or secret files.
    pub fn validate_secret_refs(&self, max_bytes: usize) -> Result<(), SecretValidationError> {
        let mut schemes = std::collections::BTreeMap::new();
        for outbound in &self.outbounds {
            let scheme = outbound
                .proxy_url
                .proxy_scheme(max_bytes)
                .map_err(|source| SecretValidationError::Resolve {
                    outbound: outbound.id.clone(),
                    source,
                })?;
            schemes.insert(outbound.id.clone(), scheme);
        }
        for upstream in &self.upstreams {
            if let ResolvedUpstream::Doh {
                id,
                bootstrap: Some(bootstrap),
                proxy: Some(proxy),
                ..
            } = upstream
                && schemes.get(proxy) == Some(&ProxyScheme::Socks5h)
            {
                return Err(SecretValidationError::BootstrapForbidden {
                    upstream: id.clone(),
                    outbound: proxy.clone(),
                    bootstrap: bootstrap.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretValidationError {
    Resolve {
        outbound: ConfigId,
        source: SecretResolveError,
    },
    BootstrapForbidden {
        upstream: ConfigId,
        outbound: ConfigId,
        bootstrap: ConfigId,
    },
}

impl fmt::Display for SecretValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve { outbound, source } => {
                write!(
                    formatter,
                    "outbound `{}` secret validation failed: {source}",
                    outbound.as_str()
                )
            }
            Self::BootstrapForbidden {
                upstream,
                outbound,
                bootstrap,
            } => write!(
                formatter,
                "upstream `{}` cannot use socks5h outbound `{}` with bootstrap `{}`",
                upstream.as_str(),
                outbound.as_str(),
                bootstrap.as_str(),
            ),
        }
    }
}

impl std::error::Error for SecretValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedactedConfigView {
    pub version: u32,
    pub work_path: PathBuf,
    pub listener_count: usize,
    pub upstream_count: usize,
    pub strategy_count: usize,
    pub input_hash: String,
    pub normalized_hash: String,
}

/// A distinct validated wrapper used at the Config → Runtime boundary.
#[derive(Clone, Debug)]
pub struct ValidatedConfig {
    pub resolved: Arc<ResolvedConfig>,
}

/// Normalize, validate and compile the current DTO into immutable runtime input.
pub fn resolve_config(
    config: &ConfigDto,
    input_hash: impl Into<String>,
) -> Result<ValidatedConfig, ConfigErrorReport> {
    validate_config(config)?;
    let bind_plan = build_bind_plan(config)?;
    let work_path = lexical_normalize(&config.work.path);
    let work = ResolvedWork {
        rules_path: resolve_path(&work_path, &config.work.rules_path),
        snapshot_path: work_path.join("config.yaml"),
        path: work_path.clone(),
    };
    let database = ResolvedDatabase {
        kind: config.database.kind,
        path: resolve_path(&work.path, &config.database.path),
    };
    let logs = ResolvedLogs {
        enable: config.logs.enable,
        level: config.logs.level,
        path: resolve_path(&work.path, &config.logs.path),
    };
    let webui = ResolvedWebUi {
        enable: config.webui.enable,
        address: config.webui.address,
        port: config.webui.port,
        users: config
            .webui
            .users
            .iter()
            .map(|user| ResolvedWebUiUser {
                name: user.name.clone(),
                password_hash: user.password_hash.clone(),
            })
            .collect(),
    };
    let dns = resolve_dns(
        config.dns.cache.as_ref(),
        config.dns.ttl_override.as_ref(),
        config.dns.edns_client_subnet.as_ref(),
        config.dns.resolve_log.as_ref(),
        &work.path,
    );
    let strategies = config
        .strategy
        .iter()
        .map(|strategy| {
            resolve_strategy(
                strategy,
                &dns.ttl_override,
                &dns.edns_client_subnet,
                &dns.cache.optimistic,
            )
        })
        .collect();
    let listeners = config
        .listener
        .iter()
        .map(|listener| resolve_listener(listener, &work.path))
        .collect();
    let upstreams = config
        .upstreams
        .iter()
        .map(|upstream| resolve_upstream(upstream, &dns.edns_client_subnet))
        .collect();
    let hosts = config
        .hosts
        .iter()
        .map(|resource| resolve_hosts(resource, &work.path))
        .collect();
    let outbounds: Vec<ResolvedOutbound> = config
        .outbound
        .iter()
        .map(|outbound| resolve_outbound(outbound, &work.path))
        .collect();
    let rule_sets = config
        .rule_set
        .iter()
        .map(|resource| resolve_rule_set(resource, &work.path))
        .collect();
    let clients = config
        .clients
        .iter()
        .map(|client| {
            resolve_client(
                client,
                &dns.ttl_override,
                &dns.edns_client_subnet,
                &dns.cache.optimistic,
            )
        })
        .collect();
    let input_hash = input_hash.into();
    let secret_material = outbounds
        .iter()
        .map(|outbound| {
            let source = match (&outbound.proxy_url.env, &outbound.proxy_url.file) {
                (Some(name), None) => format!("env:{name}"),
                (None, Some(path)) => format!("file:{path:?}"),
                _ => "invalid".to_owned(),
            };
            format!("{}={source}", outbound.id.as_str())
        })
        .collect::<Vec<_>>()
        .join("|");
    let password_material = config
        .webui
        .users
        .iter()
        .map(|user| {
            format!(
                "{}={}",
                user.name,
                deterministic_hash(user.password_hash.as_bytes())
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    let normalized_material = format!(
        "version={}|work={work:?}|db={database:?}|logs={logs:?}|webui={webui:?}|dns={dns:?}|listeners={listeners:?}|upstreams={upstreams:?}|strategies={strategies:?}|hosts={hosts:?}|outbounds={outbounds:?}|rule_sets={rule_sets:?}|clients={clients:?}|bind={bind_plan:?}|secret_refs={secret_material}|password_hashes={password_material}",
        config.version
    );
    let normalized_hash = deterministic_hash(normalized_material.as_bytes());
    Ok(ValidatedConfig {
        resolved: Arc::new(ResolvedConfig {
            version: config.version,
            work,
            database,
            logs,
            webui,
            dns,
            listeners,
            upstreams,
            strategies,
            hosts,
            outbounds,
            rule_sets,
            clients,
            bind_plan,
            input_hash,
            normalized_hash,
        }),
    })
}

fn resolve_dns(
    cache: Option<&GlobalCacheDto>,
    ttl: Option<&super::model::TtlOverrideDto>,
    ecs: Option<&EcsDto>,
    resolve_log: Option<&super::model::ResolveLogDto>,
    work_path: &Path,
) -> ResolvedDns {
    let cache = cache.map_or_else(
        || default_global_cache(work_path),
        |value| ResolvedGlobalCache {
            enabled: value.enabled,
            memory_max_size_bytes: value.memory.max_size_bytes,
            failure_ttl: value.failure_ttl,
            optimistic: resolve_optimistic(&value.optimistic),
            persistence_path: resolve_path(work_path, &value.persistence.path),
            persistence_max_size_bytes: value.persistence.max_size_bytes,
        },
    );
    ResolvedDns {
        cache,
        ttl_override: resolve_ttl(ttl, None, ValueSource::Global),
        edns_client_subnet: resolve_ecs(ecs, None, ValueSource::Global),
        resolve_log: resolve_log.map_or_else(default_resolve_log, |value| ResolvedResolveLog {
            enable: value.enable,
            eviction_threshold_records: value.eviction_threshold_records,
            max_records: value.max_records,
            max_record_age: value.max_record_age,
        }),
    }
}

fn default_global_cache(work_path: &Path) -> ResolvedGlobalCache {
    ResolvedGlobalCache {
        enabled: false,
        memory_max_size_bytes: 64 * 1024 * 1024,
        failure_ttl: Duration::from_secs(5),
        optimistic: ResolvedOptimistic {
            enabled: false,
            answer_ttl: Duration::from_secs(10),
            max_age: Duration::from_secs(86_400),
        },
        persistence_path: work_path.join("cache.db"),
        persistence_max_size_bytes: 8 * 1024 * 1024,
    }
}

fn default_resolve_log() -> ResolvedResolveLog {
    ResolvedResolveLog {
        enable: false,
        eviction_threshold_records: 90_000,
        max_records: 100_000,
        max_record_age: Duration::from_secs(7 * 86_400),
    }
}

fn resolve_optimistic(value: &OptimisticDto) -> ResolvedOptimistic {
    ResolvedOptimistic {
        enabled: value.enabled,
        answer_ttl: value.answer_ttl,
        max_age: value.max_age,
    }
}

fn resolve_cache(
    value: Option<&CacheOverrideDto>,
    parent_optimistic: &ResolvedOptimistic,
    source: ValueSource,
) -> Option<ResolvedCacheOverride> {
    value.map(|value| ResolvedCacheOverride {
        enabled: value.enabled.unwrap_or(false),
        optimistic: Some(
            value
                .optimistic
                .as_ref()
                .map_or_else(|| parent_optimistic.clone(), resolve_optimistic),
        ),
        source,
    })
}

fn resolve_ttl(
    value: Option<&super::model::TtlOverrideDto>,
    parent: Option<&ResolvedTtlOverride>,
    source: ValueSource,
) -> ResolvedTtlOverride {
    let parent = parent.cloned().unwrap_or(ResolvedTtlOverride {
        enabled: false,
        min: None,
        max: None,
        source: ValueSource::Default,
    });
    let Some(value) = value else { return parent };
    ResolvedTtlOverride {
        enabled: value.enabled.unwrap_or(parent.enabled),
        min: value.min.or(parent.min),
        max: value.max.or(parent.max),
        source,
    }
}

fn resolve_ecs(
    value: Option<&EcsDto>,
    parent: Option<&ResolvedEcs>,
    source: ValueSource,
) -> ResolvedEcs {
    let parent = parent.cloned().unwrap_or(ResolvedEcs {
        mode: EcsMode::Disabled,
        custom_ip: None,
        source: ValueSource::Default,
    });
    let Some(value) = value else { return parent };
    match value.mode {
        EcsMode::Disabled => ResolvedEcs {
            mode: EcsMode::Disabled,
            custom_ip: None,
            source,
        },
        EcsMode::Client => ResolvedEcs {
            mode: EcsMode::Client,
            custom_ip: None,
            source,
        },
        EcsMode::Custom => ResolvedEcs {
            mode: EcsMode::Custom,
            custom_ip: value.custom_ip,
            source,
        },
    }
}

fn resolve_strategy(
    strategy: &StrategyDto,
    global_ttl: &ResolvedTtlOverride,
    global_ecs: &ResolvedEcs,
    global_optimistic: &ResolvedOptimistic,
) -> ResolvedStrategy {
    let strategy_ecs = resolve_ecs(
        strategy.edns_client_subnet.as_ref(),
        Some(global_ecs),
        ValueSource::Strategy,
    );
    ResolvedStrategy {
        id: ConfigId::new(strategy.name.clone()).expect("validated strategy id"),
        rules: strategy
            .rules
            .iter()
            .map(|rule| ResolvedStrategyRule {
                rule_set: rule
                    .rule_set
                    .as_ref()
                    .map(|value| resolve_rule_set_ref(value)),
                hosts: rule
                    .hosts
                    .as_ref()
                    .map(|value| ConfigId::new(value.clone()).expect("validated hosts id")),
                upstream: rule
                    .upstream
                    .as_ref()
                    .map(|value| ConfigId::new(value.clone()).expect("validated upstream id")),
                edns_client_subnet: resolve_ecs(
                    rule.edns_client_subnet.as_ref(),
                    Some(&strategy_ecs),
                    ValueSource::Rule,
                ),
            })
            .collect(),
        default_upstream: ConfigId::new(strategy.default_upstream.clone())
            .expect("validated upstream id"),
        cache: resolve_cache(
            strategy.cache.as_ref(),
            global_optimistic,
            ValueSource::Strategy,
        ),
        ttl_override: resolve_ttl(
            strategy.ttl_override.as_ref(),
            Some(global_ttl),
            ValueSource::Strategy,
        ),
        edns_client_subnet: strategy_ecs,
    }
}

fn resolve_rule_set_ref(value: &str) -> ResolvedRuleSetRef {
    let (resource, selector) = value
        .split_once(':')
        .map_or((value, None), |(resource, selector)| {
            (resource, Some(selector.to_owned()))
        });
    ResolvedRuleSetRef {
        resource: ConfigId::new(resource.to_owned()).expect("validated rule_set reference"),
        selector,
    }
}

fn resolve_listener(listener: &ListenerDto, work_path: &Path) -> ResolvedListener {
    match listener {
        ListenerDto::Udp {
            name,
            addresses,
            port,
            strategy,
            hosts,
        } => ResolvedListener::Udp {
            id: ConfigId::new(name.clone()).expect("validated listener id"),
            addresses: addresses.clone(),
            port: *port,
            strategy: ConfigId::new(strategy.clone()).expect("validated strategy id"),
            hosts: hosts
                .as_ref()
                .map(|value| ConfigId::new(value.clone()).expect("validated hosts id")),
        },
        ListenerDto::Tcp {
            name,
            addresses,
            port,
            strategy,
            hosts,
        } => ResolvedListener::Tcp {
            id: ConfigId::new(name.clone()).expect("validated listener id"),
            addresses: addresses.clone(),
            port: *port,
            strategy: ConfigId::new(strategy.clone()).expect("validated strategy id"),
            hosts: hosts
                .as_ref()
                .map(|value| ConfigId::new(value.clone()).expect("validated hosts id")),
        },
        ListenerDto::Doh {
            name,
            routes,
            endpoints,
        } => ResolvedListener::Doh {
            id: ConfigId::new(name.clone()).expect("validated listener id"),
            routes: routes
                .iter()
                .map(|route| ResolvedDohRoute {
                    path: route.path.clone(),
                    strategy: ConfigId::new(route.strategy.clone()).expect("validated strategy id"),
                    edns_client_subnet: None,
                })
                .collect(),
            endpoints: endpoints
                .iter()
                .map(|endpoint| ResolvedDohEndpoint {
                    id: ConfigId::new(endpoint.name.clone()).expect("validated endpoint id"),
                    binding: DohBindingRef {
                        listener_id: name.clone(),
                        endpoint_id: endpoint.name.clone(),
                    },
                    addresses: endpoint.addresses.clone(),
                    port: endpoint.port,
                    tls_mode: endpoint.tls.mode,
                    certificate_file: endpoint
                        .tls
                        .certificate_file
                        .as_ref()
                        .map(|value| resolve_path(work_path, value)),
                    private_key_file: endpoint
                        .tls
                        .private_key_file
                        .as_ref()
                        .map(|value| resolve_path(work_path, value)),
                    client_ip: resolve_client_ip(&endpoint.client_ip),
                })
                .collect(),
        },
    }
}

fn resolve_client_ip(value: &ClientIpDto) -> ResolvedClientIp {
    ResolvedClientIp {
        source: value.source,
        header: value.header,
        trusted_proxies: value.trusted_proxies.clone(),
        on_missing: value.on_missing,
        on_invalid: value.on_invalid,
    }
}

fn resolve_upstream(upstream: &UpstreamDto, global_ecs: &ResolvedEcs) -> ResolvedUpstream {
    match upstream {
        UpstreamDto::Hosts {
            name,
            format,
            hosts,
        } => ResolvedUpstream::Hosts {
            id: ConfigId::new(name.clone()).expect("validated upstream id"),
            format: format.clone(),
            hosts: hosts.clone(),
        },
        UpstreamDto::Doh {
            name,
            address,
            bootstrap,
            connect_ip,
            proxy,
            edns_client_subnet,
        } => ResolvedUpstream::Doh {
            id: ConfigId::new(name.clone()).expect("validated upstream id"),
            address: address.clone(),
            bootstrap: bootstrap
                .as_ref()
                .map(|value| ConfigId::new(value.clone()).expect("validated upstream id")),
            connect_ip: *connect_ip,
            proxy: proxy
                .as_ref()
                .map(|value| ConfigId::new(value.clone()).expect("validated outbound id")),
            edns_client_subnet: Some(resolve_ecs(
                edns_client_subnet.as_ref(),
                Some(global_ecs),
                ValueSource::Upstream,
            )),
        },
        UpstreamDto::Group {
            name,
            upstreams,
            upstream_mode,
            timeout,
            fallbacks,
            fallback_upstream_mode,
            fallback_timeout,
        } => ResolvedUpstream::Group {
            id: ConfigId::new(name.clone()).expect("validated upstream id"),
            upstreams: upstreams
                .iter()
                .map(|member| ResolvedUpstreamMember {
                    name: ConfigId::new(member.name.clone()).expect("validated upstream id"),
                    weight: member.weight,
                })
                .collect(),
            upstream_mode: *upstream_mode,
            timeout: *timeout,
            fallbacks: fallbacks
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|member| ResolvedUpstreamMember {
                    name: ConfigId::new(member.name.clone()).expect("validated upstream id"),
                    weight: member.weight,
                })
                .collect(),
            fallback_upstream_mode: *fallback_upstream_mode,
            fallback_timeout: *fallback_timeout,
        },
    }
}

fn resolve_hosts(resource: &HostsResourceDto, work_path: &Path) -> ResolvedHostsResource {
    match resource {
        HostsResourceDto::Const {
            name,
            format,
            hosts,
        } => ResolvedHostsResource::Const {
            id: ConfigId::new(name.clone()).expect("validated hosts id"),
            format: *format,
            hosts: hosts.clone(),
        },
        HostsResourceDto::File {
            name,
            format,
            path,
            auto_update,
            update_interval,
        } => ResolvedHostsResource::File {
            id: ConfigId::new(name.clone()).expect("validated hosts id"),
            format: *format,
            path: resolve_path(work_path, path),
            auto_update: *auto_update,
            update_interval: *update_interval,
        },
    }
}

fn resolve_outbound(outbound: &OutboundDto, work_path: &Path) -> ResolvedOutbound {
    ResolvedOutbound {
        id: ConfigId::new(outbound.name.clone()).expect("validated outbound id"),
        kind: outbound.kind,
        proxy_url: ResolvedSecretRef {
            env: outbound.proxy_url.env.clone(),
            file: outbound
                .proxy_url
                .file
                .as_ref()
                .map(|value| resolve_path(work_path, value)),
        },
    }
}

fn resolve_rule_set(resource: &RuleSetDto, work_path: &Path) -> ResolvedRuleSet {
    match resource {
        RuleSetDto::Const { name, format, rule } => ResolvedRuleSet::Const {
            id: ConfigId::new(name.clone()).expect("validated rule_set id"),
            format: *format,
            rule: rule.clone(),
        },
        RuleSetDto::File {
            name,
            format,
            path,
            auto_update,
            update_interval,
        } => ResolvedRuleSet::File {
            id: ConfigId::new(name.clone()).expect("validated rule_set id"),
            format: *format,
            path: resolve_path(work_path, path),
            auto_update: *auto_update,
            update_interval: *update_interval,
        },
        RuleSetDto::Remote {
            name,
            format,
            url,
            proxy,
            auto_update,
            update_interval,
        } => ResolvedRuleSet::Remote {
            id: ConfigId::new(name.clone()).expect("validated rule_set id"),
            format: *format,
            url: url.clone(),
            proxy: proxy
                .as_ref()
                .map(|value| ConfigId::new(value.clone()).expect("validated outbound id")),
            auto_update: *auto_update,
            update_interval: *update_interval,
        },
    }
}

fn resolve_client(
    client: &ClientDto,
    global_ttl: &ResolvedTtlOverride,
    global_ecs: &ResolvedEcs,
    global_optimistic: &ResolvedOptimistic,
) -> ResolvedClient {
    ResolvedClient {
        id: ConfigId::new(client.name.clone()).expect("validated client id"),
        ids: client.r#match.ids.clone(),
        ips: client.r#match.ips.clone(),
        strategy: client
            .strategy
            .as_ref()
            .map(|value| ConfigId::new(value.clone()).expect("validated strategy id")),
        cache: resolve_cache(
            client.cache.as_ref(),
            global_optimistic,
            ValueSource::Client,
        ),
        ttl_override: resolve_ttl(
            client.ttl_override.as_ref(),
            Some(global_ttl),
            ValueSource::Client,
        ),
        edns_client_subnet: resolve_ecs(
            client.edns_client_subnet.as_ref(),
            Some(global_ecs),
            ValueSource::Client,
        ),
    }
}

fn resolve_path(base: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        lexical_normalize(value)
    } else {
        lexical_normalize(&base.join(value))
    }
}

/// Normalize `.` and `..` without requiring the path to exist.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() && !path.is_absolute() {
                    output.push(component.as_os_str());
                }
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                output.push(component.as_os_str());
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{ResolvedSecretRef, ResolvedSecretValue, SecretResolveError, lexical_normalize};

    #[test]
    fn lexical_path_normalization_does_not_require_existing_files() {
        assert_eq!(
            lexical_normalize(Path::new("/tmp/fluxdns/../rules/./x")),
            Path::new("/tmp/rules/x")
        );
    }

    #[test]
    fn secret_file_resolution_is_bounded_and_redacted() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("fluxdns-secret-{suffix}"));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("proxy-url");
        fs::write(&path, b"  socks5h://user:password@example.test:1080\n").unwrap();
        let reference = ResolvedSecretRef {
            env: None,
            file: Some(path.clone()),
        };
        let value = reference.resolve_proxy_url(1024).unwrap();
        assert_eq!(value.expose(), b"socks5h://user:password@example.test:1080");
        assert_eq!(format!("{value:?}"), "SecretValue([REDACTED])");
        assert_eq!(format!("{value}"), "[REDACTED]");
        assert!(!format!("{reference:?}").contains("password"));
        assert!(matches!(
            reference.resolve(8),
            Err(SecretResolveError::TooLarge { limit: 8 })
        ));
        fs::write(&path, b"https://example.test").unwrap();
        assert!(matches!(
            reference.resolve_proxy_url(1024),
            Err(SecretResolveError::UnsupportedProxyScheme)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn secret_value_type_does_not_expose_bytes_through_debug() {
        let value = ResolvedSecretValue(Box::from(&b"private"[..]));
        assert!(!format!("{value:?}").contains("private"));
    }
}
