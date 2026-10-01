//! v2 配置复用的资源 DTO、协议类型和严格字段解析。

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ipnet::IpNet;
use serde::de::{self, DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use url::Url;

pub type DohUpstreamDetails<'a> = (
    &'a Url,
    Option<&'a String>,
    Option<IpAddr>,
    Option<&'a String>,
    Option<&'a EcsDto>,
);

/// 规则资源读取、下载和解析的默认有界大小，单位为字节。
pub const DEFAULT_RULE_SET_MAX_SIZE_BYTES: usize = 16 * 1024 * 1024;
/// 每个规则资源读取、解析和索引的默认规则数量上限。
pub const DEFAULT_RULE_SET_MAX_RULES: usize = 131_072;

fn default_rule_set_max_size_bytes() -> usize {
    DEFAULT_RULE_SET_MAX_SIZE_BYTES
}

fn default_rule_set_max_rules() -> usize {
    DEFAULT_RULE_SET_MAX_RULES
}

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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDto {
    pub path: PathBuf,
    pub rules_path: PathBuf,
    #[serde(default = "default_rule_set_max_size_bytes")]
    pub rule_set_max_size_bytes: usize,
    #[serde(default = "default_rule_set_max_rules")]
    pub rule_set_max_rules: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseType {
    Sqlite,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogsDto {
    pub enable: bool,
    pub level: LogLevelDto,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMemoryDto {
    pub max_size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OptimisticDto {
    pub enabled: bool,
    #[serde(
        deserialize_with = "deserialize_duration",
        serialize_with = "serialize_duration"
    )]
    pub answer_ttl: Duration,
    #[serde(
        deserialize_with = "deserialize_duration",
        serialize_with = "serialize_duration"
    )]
    pub max_age: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheOverrideDto {
    /// Optional here so the validator can distinguish a missing `enabled` field.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled: Option<bool>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub optimistic: Option<OptimisticDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TtlOverrideDto {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled: Option<bool>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_duration",
        serialize_with = "serialize_optional_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub min: Option<Duration>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_duration",
        serialize_with = "serialize_optional_duration",
        skip_serializing_if = "Option::is_none"
    )]
    pub max: Option<Duration>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EcsDto {
    pub mode: EcsMode,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_cidr",
        skip_serializing_if = "Option::is_none"
    )]
    pub custom_ip: Option<IpNet>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EcsMode {
    Disabled,
    Client,
    Custom,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ListenerDto {
    #[serde(rename = "udp")]
    Udp {
        name: String,
        #[serde(deserialize_with = "deserialize_ip_vec")]
        addresses: Vec<IpAddr>,
        port: u16,
        strategy: String,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        hosts: Option<String>,
    },
    #[serde(rename = "tcp")]
    Tcp {
        name: String,
        #[serde(deserialize_with = "deserialize_ip_vec")]
        addresses: Vec<IpAddr>,
        port: u16,
        strategy: String,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DohRouteDto {
    pub path: String,
    pub strategy: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DohEndpointDto {
    pub name: String,
    #[serde(deserialize_with = "deserialize_ip_vec")]
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    pub tls: TlsDto,
    pub client_ip: ClientIpDto,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsDto {
    pub mode: TlsMode,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub certificate_file: Option<PathBuf>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub private_key_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    Terminate,
    External,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIpDto {
    pub source: ClientIpSource,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub header: Option<ForwardedHeader>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_cidr_vec",
        skip_serializing_if = "Option::is_none"
    )]
    pub trusted_proxies: Option<Vec<IpNet>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub on_missing: Option<ForwardedDisposition>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub on_invalid: Option<ForwardedDisposition>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientIpSource {
    Peer,
    ForwardedHeader,
    ProxyProtocol,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ForwardedHeader {
    #[serde(rename = "X-Forwarded-For")]
    XForwardedFor,
    #[serde(rename = "X-Real-IP")]
    XRealIp,
    #[serde(rename = "Forwarded")]
    Forwarded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardedDisposition {
    Reject,
    UsePeer,
}

/// Upstream variants with strict per-type fields.
#[derive(Clone, Deserialize, Serialize)]
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
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        bootstrap: Option<String>,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_ip",
            skip_serializing_if = "Option::is_none"
        )]
        connect_ip: Option<IpAddr>,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        proxy: Option<String>,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        edns_client_subnet: Option<EcsDto>,
    },
    #[serde(rename = "group")]
    Group {
        name: String,
        upstreams: Vec<UpstreamMemberDto>,
        upstream_mode: UpstreamMode,
        #[serde(
            deserialize_with = "deserialize_duration",
            serialize_with = "serialize_duration"
        )]
        timeout: Duration,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        fallbacks: Option<Vec<UpstreamMemberDto>>,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        fallback_upstream_mode: Option<UpstreamMode>,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_duration",
            serialize_with = "serialize_optional_duration",
            skip_serializing_if = "Option::is_none"
        )]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamMemberDto {
    pub name: String,
    #[serde(default = "default_upstream_member_weight")]
    pub weight: u32,
}

const fn default_upstream_member_weight() -> u32 {
    1
}

pub(crate) const MAX_RULE_SET_SELECTOR_BYTES: usize = 128;

/// 将 dat selector 规范化为配置引用和资源索引共用的 canonical key。
pub(crate) fn normalize_rule_set_selector(value: &str, max_bytes: usize) -> Option<String> {
    if value.is_empty() || value.len() > max_bytes || !value.is_ascii() {
        return None;
    }
    value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b':' | b'@'))
        .then(|| value.to_ascii_lowercase())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpstreamMode {
    Parallel,
    RoundRobin,
    LoadBalance,
    Failover,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyDto {
    pub name: String,
    pub rules: Vec<StrategyRuleDto>,
    pub default_upstream: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache: Option<CacheOverrideDto>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub ttl_override: Option<TtlOverrideDto>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub edns_client_subnet: Option<EcsDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyRuleDto {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub rule_set: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub hosts: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub upstream: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub edns_client_subnet: Option<EcsDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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
        #[serde(
            default,
            deserialize_with = "deserialize_optional_duration",
            serialize_with = "serialize_optional_duration",
            skip_serializing_if = "Option::is_none"
        )]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostsFormat {
    Json,
    Hosts,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundDto {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: OutboundType,
    pub proxy_url: SecretRefDto,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboundType {
    Socks5,
}

#[derive(Clone, Deserialize, Serialize)]
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
        #[serde(
            default,
            deserialize_with = "deserialize_optional_duration",
            serialize_with = "serialize_optional_duration",
            skip_serializing_if = "Option::is_none"
        )]
        update_interval: Option<Duration>,
    },
    #[serde(rename = "remote")]
    Remote {
        name: String,
        format: RuleSetFormat,
        #[serde(deserialize_with = "deserialize_url")]
        url: Url,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        proxy: Option<String>,
        #[serde(default)]
        auto_update: bool,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_duration",
            serialize_with = "serialize_optional_duration",
            skip_serializing_if = "Option::is_none"
        )]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleSetFormat {
    Json,
    Clash,
    Dat,
}

/// 秘密来源引用：`env`、`file`、`url` 三选一，`url` 表示直接写入的完整代理 URL。
///
/// 保留非法字段组合而不是在反序列化期报错，是为了让 `validate` 统一给出“exactly one of”
/// 诊断；Debug、日志和序列化输出只暴露脱敏后的来源，实际值只能由显式 accessor 读取。
#[derive(Clone, Eq, PartialEq)]
pub enum SecretRefDto {
    /// 从环境变量读取完整秘密值。
    Env { env: String },
    /// 从文件读取完整秘密值。
    File { file: PathBuf },
    /// 直接内联完整秘密值，例如 `socks5://user:password@host:1080`。
    Inline { url: String },
    /// 字段组合不满足三选一，仅用于把校验失败传递给上层。
    Parts {
        env: Option<String>,
        file: Option<PathBuf>,
        url: Option<String>,
    },
}

/// 内联 URL 脱敏后替换密码的占位符；回填原样提交时据此恢复真实密码。
///
/// 只使用 URL userinfo 不必转义的字符，保证经 `Url::set_password` 后逐字不变，
/// 既能被前端原样回填，也不会与真实密码在往返中产生歧义。
pub const REDACTED_INLINE_PASSWORD: &str = "FLUXDNS_REDACTED_SECRET";

/// 只遮蔽 URL 的密码部分，保留 scheme、用户名、主机和端口，便于在 WebUI 中辨认来源。
///
/// URL 无法解析或本就没有密码时原样返回：无法解析的值同样不会被恢复，避免静默改写内容。
pub fn redact_inline_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value.trim()) else {
        return value.to_owned();
    };
    if url.password().is_none_or(str::is_empty) {
        return value.to_owned();
    }
    if url.set_password(Some(REDACTED_INLINE_PASSWORD)).is_err() {
        return value.to_owned();
    }
    url.to_string()
}

impl SecretRefDto {
    /// 内联来源经脱敏后仍是普通字符串，因此 JSON/YAML 用字符串表达；其余来源保持对象。
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline { .. })
    }

    /// 未编辑的内联地址在提交时仍是脱敏值，用原值替换，避免每次编辑都要求重输密码。
    ///
    /// 只在 scheme、主机、端口与原值一致时替换密码；其余情况视为用户新输入的值。
    pub fn restore_redacted_url(&mut self, original: &Self) {
        let Self::Inline { url } = self else {
            return;
        };
        let Self::Inline { url: previous } = original else {
            return;
        };
        // 只替换密码片段，保留原字符串的其余表达，避免 Url 归一化改变端口或尾斜杠。
        let marker = format!(":{REDACTED_INLINE_PASSWORD}@");
        let Some(marker_start) = url.find(&marker) else {
            return;
        };
        let Ok(parsed) = Url::parse(url.trim()) else {
            return;
        };
        let Ok(previous_url) = Url::parse(previous.trim()) else {
            return;
        };
        let (Some(host), Some(previous_host)) = (parsed.host_str(), previous_url.host_str()) else {
            return;
        };
        // SOCKS5 的默认端口与 outbound profile 一致取 1080，缺省端口才能与原值等价。
        if parsed.scheme() != previous_url.scheme()
            || host != previous_host
            || parsed.port().unwrap_or(1080) != previous_url.port().unwrap_or(1080)
        {
            return;
        }
        let Some(password) = previous_url.password().filter(|value| !value.is_empty()) else {
            return;
        };
        let password_start = marker_start + 1;
        let password_end = password_start + REDACTED_INLINE_PASSWORD.len();
        let mut restored = String::with_capacity(url.len() + password.len());
        restored.push_str(&url[..password_start]);
        restored.push_str(password);
        restored.push_str(&url[password_end..]);
        *url = restored;
    }
}

impl fmt::Debug for SecretRefDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretRef([REDACTED])")
    }
}

impl<'de> Deserialize<'de> for SecretRefDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reference {
            #[serde(default, deserialize_with = "deserialize_optional_non_null")]
            env: Option<String>,
            #[serde(default, deserialize_with = "deserialize_optional_non_null")]
            file: Option<PathBuf>,
            #[serde(default, deserialize_with = "deserialize_optional_non_null")]
            url: Option<String>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Inline(String),
            Reference(Reference),
        }

        match Wire::deserialize(deserializer)? {
            Wire::Inline(url) => Ok(Self::Inline { url }),
            Wire::Reference(reference) => {
                Ok(match (reference.env, reference.file, reference.url) {
                    (Some(env), None, None) => Self::Env { env },
                    (None, Some(file), None) => Self::File { file },
                    (None, None, Some(url)) => Self::Inline { url },
                    (env, file, url) => Self::Parts { env, file, url },
                })
            }
        }
    }
}

impl Serialize for SecretRefDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Reference<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            env: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            file: Option<&'a Path>,
            #[serde(skip_serializing_if = "Option::is_none")]
            url: Option<&'a str>,
        }

        #[derive(Serialize)]
        #[serde(untagged)]
        enum Wire<'a> {
            Inline(String),
            Reference(Reference<'a>),
        }

        match self {
            Self::Env { env } => Wire::Reference(Reference {
                env: Some(env),
                file: None,
                url: None,
            })
            .serialize(serializer),
            Self::File { file } => Wire::Reference(Reference {
                env: None,
                file: Some(file),
                url: None,
            })
            .serialize(serializer),
            // 脱敏后的内联值仍是普通字符串，前端据此原样回填并可直接再次提交。
            Self::Inline { url } => Wire::Inline(redact_inline_url(url)).serialize(serializer),
            Self::Parts { env, file, url } => {
                let redacted = url.as_deref().map(redact_inline_url);
                let reference = Reference {
                    env: env.as_deref(),
                    file: file.as_deref(),
                    url: redacted.as_deref(),
                };
                Wire::Reference(reference).serialize(serializer)
            }
        }
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

/// 源 DTO 的 duration 使用字符串传输，不泄漏 Rust Duration 的内部字段布局。
pub(super) fn serialize_duration<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format!("{}ns", value.as_nanos()))
}

pub(super) fn serialize_optional_duration<S>(
    value: &Option<Duration>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(value) => serialize_duration(value, serializer),
        None => serializer.serialize_none(),
    }
}

pub(super) fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
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

pub(super) fn deserialize_optional_non_null<'de, D, T>(
    deserializer: D,
) -> Result<Option<T>, D::Error>
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

    use super::{
        REDACTED_INLINE_PASSWORD, SecretRefDto, WebUiDto, parse_duration, redact_inline_url,
    };

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

    #[test]
    fn inline_source_round_trips_as_a_string_while_redacting_its_password() {
        let source = "socks5://user:secret@proxy.example:1081";
        let parsed: SecretRefDto = yaml_serde::from_str(source).unwrap();
        assert!(parsed.is_inline());
        let serialized = yaml_serde::to_string(&parsed).unwrap();
        assert_eq!(
            serialized.trim(),
            format!("socks5://user:{REDACTED_INLINE_PASSWORD}@proxy.example:1081")
        );
        assert!(!serialized.contains("secret"));
        // 脱敏输出本身必须仍是合法引用，才能被前端原样回填并再次提交。
        let reparsed: SecretRefDto = yaml_serde::from_str(&serialized).unwrap();
        assert!(reparsed.is_inline());
        // 恢复只在 scheme/host/port 一致时替换密码；脱敏后的序列化输出始终不含明文，
        // 因此这里直接核对类型化值，而不是再序列化一次。
        let mut unchanged: SecretRefDto = yaml_serde::from_str(&serialized).unwrap();
        unchanged.restore_redacted_url(&parsed);
        let SecretRefDto::Inline { url: restored } = &unchanged else {
            panic!("expected an inline source");
        };
        assert_eq!(restored, source);

        // 主机不同视为用户新输入，保持脱敏值不变。
        let mut moved: SecretRefDto = yaml_serde::from_str(&format!(
            "socks5://user:{REDACTED_INLINE_PASSWORD}@other.example:1081"
        ))
        .unwrap();
        moved.restore_redacted_url(&parsed);
        let SecretRefDto::Inline { url: unchanged_url } = &moved else {
            panic!("expected an inline source");
        };
        assert_eq!(
            unchanged_url,
            &format!("socks5://user:{REDACTED_INLINE_PASSWORD}@other.example:1081")
        );
    }

    #[test]
    fn inline_and_reference_sources_are_distinguished() {
        let object: SecretRefDto = yaml_serde::from_str("{url: socks5h://proxy.example}").unwrap();
        assert_eq!(
            object,
            SecretRefDto::Inline {
                url: "socks5h://proxy.example".into()
            }
        );
        let env: SecretRefDto = yaml_serde::from_str("{env: PROXY_URL}").unwrap();
        assert_eq!(
            env,
            SecretRefDto::Env {
                env: "PROXY_URL".into()
            }
        );
        // 非法组合保留为 Parts，交由 validate 统一给出 exactly-one-of 诊断。
        let mixed: SecretRefDto =
            yaml_serde::from_str("{env: PROXY_URL, url: socks5://proxy.example}").unwrap();
        assert!(matches!(mixed, SecretRefDto::Parts { .. }));
        assert_eq!(
            redact_inline_url("socks5://user:secret@proxy.example"),
            format!("socks5://user:{REDACTED_INLINE_PASSWORD}@proxy.example")
        );
        assert_eq!(redact_inline_url("not-a-url"), "not-a-url");
    }
}
