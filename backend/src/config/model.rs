//! Strict version 1 configuration DTOs.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ipnet::IpNet;
use serde::Deserialize;
use serde::de::{self, DeserializeOwned, Deserializer};
use url::Url;

/// The schema revision implemented by this module.
pub const CURRENT_CONFIG_VERSION: u32 = 1;

/// Raw/current DTO. Semantic validation is intentionally performed after deserialization.
pub type RawConfig = ConfigDto;

pub type DohUpstreamDetails<'a> = (
    &'a Url,
    Option<&'a String>,
    Option<IpAddr>,
    Option<&'a String>,
    Option<&'a EcsDto>,
);

pub type UpstreamGroupDetails<'a> = (
    &'a [UpstreamMemberDto],
    &'a UpstreamMode,
    Duration,
    Option<&'a [UpstreamMemberDto]>,
    Option<&'a UpstreamMode>,
    Option<Duration>,
);

pub type RemoteRuleSetDetails<'a> = (
    &'a RuleSetFormat,
    &'a Url,
    Option<&'a String>,
    bool,
    Option<Duration>,
);

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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDto {
    pub version: u32,
    pub work: WorkDto,
    pub database: DatabaseDto,
    pub logs: LogsDto,
    pub webui: WebUiDto,
    pub dns: DnsDto,
    #[serde(default)]
    pub listener: Vec<ListenerDto>,
    #[serde(default)]
    pub upstreams: Vec<UpstreamDto>,
    #[serde(default)]
    pub strategy: Vec<StrategyDto>,
    #[serde(default)]
    pub hosts: Vec<HostsResourceDto>,
    #[serde(default)]
    pub outbound: Vec<OutboundDto>,
    #[serde(default)]
    pub rule_set: Vec<RuleSetDto>,
    #[serde(default)]
    pub clients: Vec<ClientDto>,
}

impl fmt::Debug for ConfigDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigDto")
            .field("version", &self.version)
            .field("work", &self.work)
            .field("database", &self.database)
            .field("logs", &self.logs)
            .field("webui", &self.webui)
            .field("dns", &self.dns)
            .field("listener", &self.listener)
            .field("upstreams", &self.upstreams)
            .field("strategy", &self.strategy)
            .field("hosts", &self.hosts)
            .field("outbound", &self.outbound)
            .field("rule_set", &self.rule_set)
            .field("clients", &self.clients)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDto {
    pub path: PathBuf,
    pub rules_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseDto {
    #[serde(rename = "type")]
    pub kind: DatabaseType,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseType {
    Sqlite,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogsDto {
    pub enable: bool,
    pub level: LogLevelDto,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevelDto {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl<'de> Deserialize<'de> for LogLevelDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?
            .to_ascii_lowercase()
            .as_str()
        {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            value => Err(de::Error::custom(format!(
                "unsupported log level `{value}`"
            ))),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebUiDto {
    pub enable: bool,
    #[serde(deserialize_with = "deserialize_ip")]
    pub address: IpAddr,
    pub port: u16,
    #[serde(default, deserialize_with = "deserialize_optional_url")]
    pub public_origin: Option<Url>,
    #[serde(default)]
    pub users: Vec<WebUiUserDto>,
}

impl fmt::Debug for WebUiDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebUiDto")
            .field("enable", &self.enable)
            .field("address", &self.address)
            .field("port", &self.port)
            .field("public_origin", &self.public_origin.as_ref().map(SafeUrl))
            .field("users", &self.users)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebUiUserDto {
    pub name: String,
    pub password_hash: String,
}

impl fmt::Debug for WebUiUserDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebUiUserDto")
            .field("name", &self.name)
            .field("password_hash", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsDto {
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub cache: Option<GlobalCacheDto>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub ttl_override: Option<TtlOverrideDto>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub edns_client_subnet: Option<EcsDto>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub resolve_log: Option<ResolveLogDto>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalCacheDto {
    pub enabled: bool,
    pub memory: CacheMemoryDto,
    #[serde(deserialize_with = "deserialize_duration")]
    pub failure_ttl: Duration,
    pub optimistic: OptimisticDto,
    pub persistence: CachePersistenceDto,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMemoryDto {
    pub max_size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePersistenceDto {
    pub path: PathBuf,
    pub max_size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimisticDto {
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_duration")]
    pub answer_ttl: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub max_age: Duration,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheOverrideDto {
    /// Optional here so the validator can distinguish a missing `enabled` field.
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub optimistic: Option<OptimisticDto>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TtlOverrideDto {
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    pub min: Option<Duration>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    pub max: Option<Duration>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcsDto {
    pub mode: EcsMode,
    #[serde(default, deserialize_with = "deserialize_optional_cidr")]
    pub custom_ip: Option<IpNet>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EcsMode {
    Disabled,
    Client,
    Custom,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveLogDto {
    pub enable: bool,
    pub eviction_threshold_records: u64,
    pub max_records: u64,
    #[serde(deserialize_with = "deserialize_duration")]
    pub max_record_age: Duration,
}

/// Listener variants are internally tagged so each variant has an independent strict field set.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ListenerDto {
    #[serde(rename = "udp")]
    Udp {
        name: String,
        #[serde(deserialize_with = "deserialize_ip_vec")]
        addresses: Vec<IpAddr>,
        port: u16,
        strategy: String,
        #[serde(default, deserialize_with = "deserialize_optional_non_null")]
        hosts: Option<String>,
    },
    #[serde(rename = "tcp")]
    Tcp {
        name: String,
        #[serde(deserialize_with = "deserialize_ip_vec")]
        addresses: Vec<IpAddr>,
        port: u16,
        strategy: String,
        #[serde(default, deserialize_with = "deserialize_optional_non_null")]
        hosts: Option<String>,
    },
    #[serde(rename = "doh")]
    Doh {
        name: String,
        routes: Vec<DohRouteDto>,
        endpoints: Vec<DohEndpointDto>,
    },
}

impl ListenerDto {
    pub fn name(&self) -> &str {
        match self {
            Self::Udp { name, .. } | Self::Tcp { name, .. } | Self::Doh { name, .. } => name,
        }
    }

    pub fn stream_details(&self) -> Option<(&[IpAddr], u16, &str, Option<&str>)> {
        match self {
            Self::Udp {
                addresses,
                port,
                strategy,
                hosts,
                ..
            }
            | Self::Tcp {
                addresses,
                port,
                strategy,
                hosts,
                ..
            } => Some((addresses, *port, strategy, hosts.as_deref())),
            Self::Doh { .. } => None,
        }
    }

    pub fn doh_details(&self) -> Option<(&[DohRouteDto], &[DohEndpointDto])> {
        match self {
            Self::Doh {
                routes, endpoints, ..
            } => Some((routes, endpoints)),
            Self::Udp { .. } | Self::Tcp { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DohRouteDto {
    pub path: String,
    pub strategy: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DohEndpointDto {
    pub name: String,
    #[serde(deserialize_with = "deserialize_ip_vec")]
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    pub tls: TlsDto,
    pub client_ip: ClientIpDto,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsDto {
    pub mode: TlsMode,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub certificate_file: Option<PathBuf>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub private_key_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    Terminate,
    External,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIpDto {
    pub source: ClientIpSource,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub header: Option<ForwardedHeader>,
    #[serde(default, deserialize_with = "deserialize_optional_cidr_vec")]
    pub trusted_proxies: Option<Vec<IpNet>>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub on_missing: Option<ForwardedDisposition>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub on_invalid: Option<ForwardedDisposition>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ClientIpSource {
    Peer,
    ForwardedHeader,
    ProxyProtocol,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum ForwardedHeader {
    #[serde(rename = "X-Forwarded-For")]
    XForwardedFor,
    #[serde(rename = "X-Real-IP")]
    XRealIp,
    #[serde(rename = "Forwarded")]
    Forwarded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ForwardedDisposition {
    Reject,
    UsePeer,
}

/// Upstream variants with strict per-type fields.
#[derive(Clone, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum UpstreamDto {
    #[serde(rename = "hosts")]
    Hosts {
        name: String,
        format: String,
        hosts: String,
    },
    #[serde(rename = "doh")]
    Doh {
        name: String,
        #[serde(deserialize_with = "deserialize_url")]
        address: Url,
        #[serde(default, deserialize_with = "deserialize_optional_non_null")]
        bootstrap: Option<String>,
        #[serde(default, deserialize_with = "deserialize_optional_ip")]
        connect_ip: Option<IpAddr>,
        #[serde(default, deserialize_with = "deserialize_optional_non_null")]
        proxy: Option<String>,
        #[serde(default, deserialize_with = "deserialize_optional_non_null")]
        edns_client_subnet: Option<EcsDto>,
    },
    #[serde(rename = "group")]
    Group {
        name: String,
        upstreams: Vec<UpstreamMemberDto>,
        upstream_mode: UpstreamMode,
        #[serde(deserialize_with = "deserialize_duration")]
        timeout: Duration,
        #[serde(default, deserialize_with = "deserialize_optional_non_null")]
        fallbacks: Option<Vec<UpstreamMemberDto>>,
        #[serde(default, deserialize_with = "deserialize_optional_non_null")]
        fallback_upstream_mode: Option<UpstreamMode>,
        #[serde(default, deserialize_with = "deserialize_optional_duration")]
        fallback_timeout: Option<Duration>,
    },
}

impl fmt::Debug for UpstreamDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hosts {
                name,
                format,
                hosts,
            } => formatter
                .debug_struct("UpstreamDto::Hosts")
                .field("name", name)
                .field("format", format)
                .field("hosts_len", &hosts.len())
                .finish(),
            Self::Doh {
                name,
                address,
                bootstrap,
                connect_ip,
                proxy,
                edns_client_subnet,
            } => formatter
                .debug_struct("UpstreamDto::Doh")
                .field("name", name)
                .field("address", &SafeUrl(address))
                .field("bootstrap", bootstrap)
                .field("connect_ip", connect_ip)
                .field("proxy", proxy)
                .field("edns_client_subnet", edns_client_subnet)
                .finish(),
            Self::Group {
                name,
                upstreams,
                upstream_mode,
                timeout,
                fallbacks,
                fallback_upstream_mode,
                fallback_timeout,
            } => formatter
                .debug_struct("UpstreamDto::Group")
                .field("name", name)
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

impl UpstreamDto {
    pub fn name(&self) -> &str {
        match self {
            Self::Hosts { name, .. } | Self::Doh { name, .. } | Self::Group { name, .. } => name,
        }
    }

    pub fn hosts_details(&self) -> Option<(&str, &str)> {
        match self {
            Self::Hosts { format, hosts, .. } => Some((format, hosts)),
            Self::Doh { .. } | Self::Group { .. } => None,
        }
    }

    pub fn doh_details(&self) -> Option<DohUpstreamDetails<'_>> {
        match self {
            Self::Doh {
                address,
                bootstrap,
                connect_ip,
                proxy,
                edns_client_subnet,
                ..
            } => Some((
                address,
                bootstrap.as_ref(),
                *connect_ip,
                proxy.as_ref(),
                edns_client_subnet.as_ref(),
            )),
            Self::Hosts { .. } | Self::Group { .. } => None,
        }
    }

    pub fn group_details(&self) -> Option<UpstreamGroupDetails<'_>> {
        match self {
            Self::Group {
                upstreams,
                upstream_mode,
                timeout,
                fallbacks,
                fallback_upstream_mode,
                fallback_timeout,
                ..
            } => Some((
                upstreams,
                upstream_mode,
                *timeout,
                fallbacks.as_deref(),
                fallback_upstream_mode.as_ref(),
                *fallback_timeout,
            )),
            Self::Hosts { .. } | Self::Doh { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamMemberDto {
    pub name: String,
    pub weight: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum UpstreamMode {
    Parallel,
    RoundRobin,
    LoadBalance,
    Failover,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyDto {
    pub name: String,
    pub rules: Vec<StrategyRuleDto>,
    pub default_upstream: String,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub cache: Option<CacheOverrideDto>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub ttl_override: Option<TtlOverrideDto>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub edns_client_subnet: Option<EcsDto>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyRuleDto {
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub rule_set: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub hosts: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub upstream: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub edns_client_subnet: Option<EcsDto>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum HostsResourceDto {
    #[serde(rename = "const")]
    Const {
        name: String,
        format: HostsFormat,
        hosts: String,
    },
    #[serde(rename = "file")]
    File {
        name: String,
        format: HostsFormat,
        path: PathBuf,
        #[serde(default)]
        auto_update: bool,
        #[serde(default, deserialize_with = "deserialize_optional_duration")]
        update_interval: Option<Duration>,
    },
}

impl HostsResourceDto {
    pub fn name(&self) -> &str {
        match self {
            Self::Const { name, .. } | Self::File { name, .. } => name,
        }
    }

    pub fn const_details(&self) -> Option<(&HostsFormat, &str)> {
        match self {
            Self::Const { format, hosts, .. } => Some((format, hosts)),
            Self::File { .. } => None,
        }
    }

    pub fn file_details(&self) -> Option<(&HostsFormat, &PathBuf, bool, Option<Duration>)> {
        match self {
            Self::File {
                format,
                path,
                auto_update,
                update_interval,
                ..
            } => Some((format, path, *auto_update, *update_interval)),
            Self::Const { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum HostsFormat {
    Json,
    Hosts,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundDto {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: OutboundType,
    pub proxy_url: SecretRefDto,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OutboundType {
    Socks5,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum RuleSetDto {
    #[serde(rename = "const")]
    Const {
        name: String,
        format: RuleSetFormat,
        rule: String,
    },
    #[serde(rename = "file")]
    File {
        name: String,
        format: RuleSetFormat,
        path: PathBuf,
        #[serde(default)]
        auto_update: bool,
        #[serde(default, deserialize_with = "deserialize_optional_duration")]
        update_interval: Option<Duration>,
    },
    #[serde(rename = "remote")]
    Remote {
        name: String,
        format: RuleSetFormat,
        #[serde(deserialize_with = "deserialize_url")]
        url: Url,
        #[serde(default, deserialize_with = "deserialize_optional_non_null")]
        proxy: Option<String>,
        #[serde(default)]
        auto_update: bool,
        #[serde(default, deserialize_with = "deserialize_optional_duration")]
        update_interval: Option<Duration>,
    },
}

impl fmt::Debug for RuleSetDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Const { name, format, rule } => formatter
                .debug_struct("RuleSetDto::Const")
                .field("name", name)
                .field("format", format)
                .field("rule_len", &rule.len())
                .finish(),
            Self::File {
                name,
                format,
                path,
                auto_update,
                update_interval,
            } => formatter
                .debug_struct("RuleSetDto::File")
                .field("name", name)
                .field("format", format)
                .field("path", path)
                .field("auto_update", auto_update)
                .field("update_interval", update_interval)
                .finish(),
            Self::Remote {
                name,
                format,
                url,
                proxy,
                auto_update,
                update_interval,
            } => formatter
                .debug_struct("RuleSetDto::Remote")
                .field("name", name)
                .field("format", format)
                .field("url", &SafeUrl(url))
                .field("proxy", proxy)
                .field("auto_update", auto_update)
                .field("update_interval", update_interval)
                .finish(),
        }
    }
}

impl RuleSetDto {
    pub fn name(&self) -> &str {
        match self {
            Self::Const { name, .. } | Self::File { name, .. } | Self::Remote { name, .. } => name,
        }
    }

    pub fn const_details(&self) -> Option<(&RuleSetFormat, &str)> {
        match self {
            Self::Const { format, rule, .. } => Some((format, rule)),
            Self::File { .. } | Self::Remote { .. } => None,
        }
    }

    pub fn file_details(&self) -> Option<(&RuleSetFormat, &PathBuf, bool, Option<Duration>)> {
        match self {
            Self::File {
                format,
                path,
                auto_update,
                update_interval,
                ..
            } => Some((format, path, *auto_update, *update_interval)),
            Self::Const { .. } | Self::Remote { .. } => None,
        }
    }

    pub fn remote_details(&self) -> Option<RemoteRuleSetDetails<'_>> {
        match self {
            Self::Remote {
                format,
                url,
                proxy,
                auto_update,
                update_interval,
                ..
            } => Some((format, url, proxy.as_ref(), *auto_update, *update_interval)),
            Self::Const { .. } | Self::File { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RuleSetFormat {
    Json,
    Clash,
    Dat,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientDto {
    pub name: String,
    pub r#match: ClientMatchDto,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub strategy: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub cache: Option<CacheOverrideDto>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub ttl_override: Option<TtlOverrideDto>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub edns_client_subnet: Option<EcsDto>,
}

impl fmt::Debug for ClientDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientDto")
            .field("name", &self.name)
            .field("match", &self.r#match)
            .field("strategy", &self.strategy)
            .field("cache", &self.cache)
            .field("ttl_override", &self.ttl_override)
            .field("edns_client_subnet", &self.edns_client_subnet)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientMatchDto {
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_cidr_vec")]
    pub ips: Vec<IpNet>,
}

impl fmt::Debug for ClientMatchDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientMatchDto")
            .field("id_count", &self.ids.len())
            .field("ip_count", &self.ips.len())
            .finish()
    }
}

/// A secret source. Its debug representation intentionally omits source details and value.
#[derive(Clone, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SecretRefDto {
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub env: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_non_null")]
    pub file: Option<PathBuf>,
}

impl fmt::Debug for SecretRefDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretRef([REDACTED])")
    }
}

/// A tri-state field useful to migration/normalization callers that need to retain YAML null.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum TriState<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<'de, T> Deserialize<'de> for TriState<T>
where
    T: DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(|value| value.map_or(Self::Null, Self::Value))
    }
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    parse_duration(&text).map_err(de::Error::custom)
}

fn deserialize_optional_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?
        .ok_or_else(|| de::Error::custom("null is not allowed; omit the field instead"))?;
    parse_duration(&value).map(Some).map_err(de::Error::custom)
}

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| de::Error::custom("null is not allowed; omit the field instead"))
}

fn deserialize_ip<'de, D>(deserializer: D) -> Result<IpAddr, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    text.parse()
        .map_err(|error| de::Error::custom(format!("invalid IP address: {error}")))
}

fn deserialize_optional_ip<'de, D>(deserializer: D) -> Result<Option<IpAddr>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?
        .ok_or_else(|| de::Error::custom("null is not allowed; omit the field instead"))?;
    value
        .parse()
        .map(Some)
        .map_err(|error| de::Error::custom(format!("invalid IP address: {error}")))
}

fn deserialize_ip_vec<'de, D>(deserializer: D) -> Result<Vec<IpAddr>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|value| {
            value
                .parse()
                .map_err(|error| de::Error::custom(format!("invalid IP address: {error}")))
        })
        .collect()
}

fn deserialize_optional_cidr<'de, D>(deserializer: D) -> Result<Option<IpNet>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?
        .ok_or_else(|| de::Error::custom("null is not allowed; omit the field instead"))?;
    value
        .parse()
        .map(Some)
        .map_err(|error| de::Error::custom(format!("invalid CIDR: {error}")))
}

fn deserialize_cidr_vec<'de, D>(deserializer: D) -> Result<Vec<IpNet>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|value| {
            value
                .parse()
                .map_err(|error| de::Error::custom(format!("invalid CIDR: {error}")))
        })
        .collect()
}

fn deserialize_optional_cidr_vec<'de, D>(deserializer: D) -> Result<Option<Vec<IpNet>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Option::<Vec<String>>::deserialize(deserializer)?
        .ok_or_else(|| de::Error::custom("null is not allowed; omit the field instead"))?;
    values
        .into_iter()
        .map(|value| {
            value
                .parse()
                .map_err(|error| de::Error::custom(format!("invalid CIDR: {error}")))
        })
        .collect::<Result<Vec<IpNet>, _>>()
        .map(Some)
}

fn deserialize_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    Url::parse(&text).map_err(|error| de::Error::custom(format!("invalid URL: {error}")))
}

fn deserialize_optional_url<'de, D>(deserializer: D) -> Result<Option<Url>, D::Error>
where
    D: Deserializer<'de>,
{
    let text = Option::<String>::deserialize(deserializer)?
        .ok_or_else(|| de::Error::custom("null is not allowed; omit the field instead"))?;
    Url::parse(&text)
        .map(Some)
        .map_err(|error| de::Error::custom(format!("invalid URL: {error}")))
}

/// Parse a compact duration such as `10s`, `1d`, or `1h30m` without platform-dependent floats.
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    let input = value.trim();
    if input.is_empty() {
        return Err("duration must not be empty".to_owned());
    }

    let bytes = input.as_bytes();
    let mut index = 0;
    let mut total_nanos = 0_u128;
    let mut components = 0_u32;

    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == number_start {
            return Err(format!("invalid duration near `{}`", &input[index..]));
        }

        let integer = input[number_start..index]
            .parse::<u128>()
            .map_err(|_| "duration number is too large".to_owned())?;
        let mut fractional = 0_u128;
        let mut fractional_digits = 0_u32;
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let fraction_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                if fractional_digits < 9 {
                    fractional = fractional * 10 + u128::from(bytes[index] - b'0');
                }
                fractional_digits += 1;
                index += 1;
            }
            if index == fraction_start || fractional_digits > 9 {
                return Err("duration fractional part must contain 1..=9 digits".to_owned());
            }
        }

        let unit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        let unit = &input[unit_start..index];
        let unit_nanos = match unit {
            "ns" => 1_u128,
            "us" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 60 * 60 * 1_000_000_000,
            "d" => 24 * 60 * 60 * 1_000_000_000,
            "w" => 7 * 24 * 60 * 60 * 1_000_000_000,
            _ => return Err(format!("unsupported duration unit `{unit}`")),
        };
        let integer_nanos = integer
            .checked_mul(unit_nanos)
            .ok_or_else(|| "duration is too large".to_owned())?;
        let fractional_nanos = if fractional_digits == 0 {
            0
        } else {
            let scale = 10_u128.pow(9 - fractional_digits);
            fractional
                .checked_mul(unit_nanos)
                .and_then(|value| value.checked_mul(scale))
                .map(|value| value / 1_000_000_000)
                .ok_or_else(|| "duration is too large".to_owned())?
        };
        total_nanos = total_nanos
            .checked_add(integer_nanos)
            .and_then(|value| value.checked_add(fractional_nanos))
            .ok_or_else(|| "duration is too large".to_owned())?;
        components += 1;
    }

    if components == 0 {
        return Err("duration must contain a numeric component".to_owned());
    }
    if total_nanos > u128::from(u64::MAX) * 1_000_000_000 {
        return Err("duration is too large".to_owned());
    }
    Ok(Duration::new(
        (total_nanos / 1_000_000_000) as u64,
        (total_nanos % 1_000_000_000) as u32,
    ))
}

/// Validate a path-like value without reading the filesystem.
pub fn is_non_empty_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::Deserialize;

    use super::{WebUiDto, parse_duration};

    #[test]
    fn parses_compact_and_compound_durations() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5_400));
        assert_eq!(
            parse_duration("1.5s").unwrap(),
            Duration::from_millis(1_500)
        );
    }

    #[test]
    fn accepts_zero_for_fields_that_use_zero_as_a_sentinel() {
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert!(parse_duration("2fortnights").is_err());
        assert!(parse_duration("1.1234567890s").is_err());
    }

    #[test]
    fn webui_users_can_be_omitted_but_not_null() {
        fn parse(source: &str) -> Result<WebUiDto, String> {
            WebUiDto::deserialize(yaml_serde::Deserializer::from_slice(source.as_bytes()))
                .map_err(|error| error.to_string())
        }

        let webui = parse(
            "enable: false\naddress: 127.0.0.1\nport: 8080\npublic_origin: http://127.0.0.1:8080\n",
        )
        .unwrap();
        assert!(webui.users.is_empty());

        let error = parse(
            "enable: false\naddress: 127.0.0.1\nport: 8080\npublic_origin: http://127.0.0.1:8080\nusers: null\n",
        )
        .unwrap_err();
        assert!(error.contains("invalid type"));
    }
}
