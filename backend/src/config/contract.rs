//! 正式 v2 配置契约；生产 loader 直接解析，不提供旧格式转换。

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ipnet::IpNet;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use super::model::{
    CacheMemoryDto, CacheOverrideDto, DatabaseType, EcsDto, HostsResourceDto, ListenerDto, LogsDto,
    OptimisticDto, OutboundDto, RuleSetDto, StrategyDto, TtlOverrideDto, UpstreamDto, WebUiDto,
    WorkDto, deserialize_duration, deserialize_optional_non_null, serialize_duration,
};
use super::resolve::lexical_normalize;
use super::validate::{
    ConfigError, ConfigErrorKind, ConfigErrorReport, ResourceConfig, validate_cache_override,
    validate_client_strategy, validate_ecs, validate_name, validate_optimistic, validate_resources,
    validate_ttl,
};

pub const CONFIG_VERSION: u32 = 2;
pub const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RESOURCES_PER_NAMESPACE: usize = 1024;
pub const MAX_INLINE_BYTES: usize = 256 * 1024;
pub const MAX_CLIENT_IPS: usize = 256;
pub const MAX_SIZE_BYTES: u64 = 1 << 40;
pub const MAX_RETENTION_DAYS: u32 = 3650;

/// 保留源配置的可选覆盖项；完整语法表达和注释仍须由 ConfigStore 持有。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigV2 {
    pub version: u32,
    pub work: WorkDto,
    pub database: DatabaseV2,
    pub logs: LogsDto,
    pub webui: WebUiDto,
    pub dns: DnsV2,
    #[serde(default)]
    pub statistics: StatisticsV2,
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
    pub clients: Vec<ClientV2>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseV2 {
    #[serde(rename = "type")]
    pub kind: DatabaseType,
    pub path: PathBuf,
    pub records_path: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DnsV2 {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache: Option<GlobalCacheV2>,
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
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub resolve_log: Option<ResolveLogV2>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalCacheV2 {
    pub enabled: bool,
    pub memory: CacheMemoryDto,
    #[serde(
        deserialize_with = "deserialize_duration",
        serialize_with = "serialize_duration"
    )]
    pub failure_ttl: Duration,
    pub optimistic: OptimisticDto,
    #[serde(default)]
    pub persistence: SnapshotV2,
}

impl Default for GlobalCacheV2 {
    fn default() -> Self {
        Self {
            enabled: false,
            memory: CacheMemoryDto {
                max_size_bytes: 64 * 1024 * 1024,
            },
            failure_ttl: Duration::from_secs(5),
            optimistic: OptimisticDto {
                enabled: false,
                answer_ttl: Duration::from_secs(10),
                max_age: Duration::from_secs(86400),
            },
            persistence: SnapshotV2::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotV2 {
    pub enabled: bool,
    pub path: PathBuf,
    #[serde(
        deserialize_with = "deserialize_duration",
        serialize_with = "serialize_duration"
    )]
    pub snapshot_interval: Duration,
}

impl Default for SnapshotV2 {
    fn default() -> Self {
        Self {
            enabled: false,
            path: PathBuf::from("./data/dns-cache.db"),
            snapshot_interval: Duration::from_secs(300),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveLogV2 {
    pub enable: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatisticsV2 {
    pub retention: RetentionV2,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionV2 {
    pub days: u32,
    pub grace_days: u32,
    pub reference_size_bytes: u64,
}

impl Default for RetentionV2 {
    fn default() -> Self {
        Self {
            days: 7,
            grace_days: 3,
            reference_size_bytes: 1 << 30,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientV2 {
    pub name: String,
    pub client_id: String,
    #[serde(default)]
    pub r#match: ClientMatchV2,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub strategy: Option<String>,
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

impl std::fmt::Debug for ClientV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientV2")
            .field("name", &self.name)
            .field("client_id", &"[REDACTED]")
            .field("ip_count", &self.r#match.ips.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientMatchV2 {
    #[serde(default, deserialize_with = "deserialize_client_ips")]
    pub ips: Vec<IpNet>,
}

/// 单地址、CIDR 和 IPv4-mapped IPv6 使用同一规范化键，重复规则不能依赖列表顺序。
pub fn parse_client_ip(value: &str) -> Result<IpNet, &'static str> {
    let network = value
        .parse::<IpNet>()
        .or_else(|_| value.parse::<IpAddr>().map(IpNet::from))
        .map_err(|_| "invalid IP or CIDR")?;
    if let IpNet::V6(v6) = network
        && let Some(ip) = v6.addr().to_ipv4_mapped()
    {
        if v6.prefix_len() < 96 {
            return Err("IPv4-mapped CIDR prefix must be at least 96");
        }
        return IpNet::new(ip.into(), v6.prefix_len() - 96)
            .map(|value| value.trunc())
            .map_err(|_| "invalid mapped CIDR");
    }
    Ok(network.trunc())
}

fn deserialize_client_ips<'de, D>(deserializer: D) -> Result<Vec<IpNet>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|value| parse_client_ip(&value).map_err(de::Error::custom))
        .collect()
}

/// client_id 大小写敏感，使用 URL unreserved ASCII；不从 name 派生，也不做大小写折叠。
pub fn valid_client_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
}

impl ConfigV2 {
    /// 有界严格解析并执行无 I/O 语义检查；错误不回显 YAML、Secret 或用户输入值。
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigErrorReport> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(single_error(
                ConfigErrorKind::InvalidValue,
                "$",
                "configuration exceeds 4 MiB",
            ));
        }
        #[derive(Deserialize, Serialize)]
        struct Version {
            version: u32,
        }
        let header: Version = yaml_serde::from_slice(bytes).map_err(|_| {
            single_error(
                ConfigErrorKind::InvalidValue,
                "version",
                "invalid configuration version",
            )
        })?;
        if header.version != CONFIG_VERSION {
            return Err(single_error(
                ConfigErrorKind::UnsupportedVersion,
                "version",
                "only version 2 is accepted; use a new development directory",
            ));
        }
        // YAML 字符串 visitor 可能把裸 null 转成文本；进入 DTO 前统一拒绝空值。
        let tree: yaml_serde::Value = yaml_serde::from_slice(bytes).map_err(|_| {
            single_error(
                ConfigErrorKind::InvalidValue,
                "$",
                "invalid configuration document",
            )
        })?;
        reject_null(&tree, "$", 0)?;
        let value: Self = serde_path_to_error::deserialize(yaml_serde::Deserializer::from_slice(
            bytes,
        ))
        .map_err(|error| {
            single_error(
                ConfigErrorKind::InvalidValue,
                &error.path().to_string(),
                "invalid v2 configuration shape",
            )
        })?;
        value.validate()?;
        Ok(value)
    }

    /// 校验新字段及共享引用图，不向旧配置 DTO 填充占位值，也不访问资源或文件。
    pub fn validate(&self) -> Result<(), ConfigErrorReport> {
        let mut report = ConfigErrorReport::default();
        if self.version != CONFIG_VERSION {
            report.push(ConfigError::new(
                ConfigErrorKind::UnsupportedVersion,
                "version",
                "only version 2 is accepted; use a new development directory",
            ));
        }
        let resources = ResourceConfig {
            work: &self.work,
            database_path: &self.database.path,
            logs: &self.logs,
            webui: &self.webui,
            listener: &self.listener,
            upstreams: &self.upstreams,
            strategy: &self.strategy,
            hosts: &self.hosts,
            outbound: &self.outbound,
            rule_set: &self.rule_set,
        };
        validate_resources(&resources, &mut report);
        for (path, count) in [
            ("listener", self.listener.len()),
            ("upstreams", self.upstreams.len()),
            ("strategy", self.strategy.len()),
            ("hosts", self.hosts.len()),
            ("outbound", self.outbound.len()),
            ("rule_set", self.rule_set.len()),
            ("clients", self.clients.len()),
        ] {
            check(
                count <= MAX_RESOURCES_PER_NAMESPACE,
                path,
                "namespace exceeds 1024 resources",
                &mut report,
            );
        }
        for (index, hosts) in self.hosts.iter().enumerate() {
            if let HostsResourceDto::Const { hosts, .. } = hosts {
                check(
                    hosts.len() <= MAX_INLINE_BYTES,
                    &format!("hosts[{index}].hosts"),
                    "inline text exceeds 256 KiB",
                    &mut report,
                );
            }
        }
        for (index, rule_set) in self.rule_set.iter().enumerate() {
            if let RuleSetDto::Const { rule, .. } = rule_set {
                check(
                    rule.len() <= MAX_INLINE_BYTES,
                    &format!("rule_set[{index}].rule"),
                    "inline text exceeds 256 KiB",
                    &mut report,
                );
            }
        }
        for (index, upstream) in self.upstreams.iter().enumerate() {
            if let UpstreamDto::Hosts { hosts, .. } = upstream {
                check(
                    hosts.len() <= MAX_INLINE_BYTES,
                    &format!("upstreams[{index}].hosts"),
                    "inline text exceeds 256 KiB",
                    &mut report,
                );
            }
        }
        let cache = self.dns.cache.clone().unwrap_or_default();
        validate_size(
            cache.memory.max_size_bytes,
            "dns.cache.memory.max_size_bytes",
            &mut report,
        );
        check(
            (Duration::from_secs(1)..=Duration::from_secs(300)).contains(&cache.failure_ttl),
            "dns.cache.failure_ttl",
            "failure TTL must be in 1s..=5m",
            &mut report,
        );
        validate_optimistic(&cache.optimistic, "dns.cache.optimistic", &mut report);
        check(
            (Duration::from_secs(1)..=Duration::from_secs(86400))
                .contains(&cache.persistence.snapshot_interval),
            "dns.cache.persistence.snapshot_interval",
            "snapshot interval must be in 1s..=1d",
            &mut report,
        );
        check(
            !cache.persistence.path.as_os_str().is_empty(),
            "dns.cache.persistence.path",
            "path is required",
            &mut report,
        );
        check(
            !self.database.records_path.as_os_str().is_empty(),
            "database.records_path",
            "path is required",
            &mut report,
        );
        if let Some(ttl) = &self.dns.ttl_override {
            validate_ttl(ttl, "dns.ttl_override", &mut report);
        }
        validate_ecs(
            self.dns.edns_client_subnet.as_ref(),
            "dns.edns_client_subnet",
            &mut report,
        );
        let retention = &self.statistics.retention;
        check(
            retention.days > 0 && retention.days <= MAX_RETENTION_DAYS,
            "statistics.retention.days",
            "days must be in 1..=3650",
            &mut report,
        );
        check(
            retention
                .days
                .checked_add(retention.grace_days)
                .is_some_and(|sum| sum <= MAX_RETENTION_DAYS),
            "statistics.retention.grace_days",
            "days plus grace_days must not exceed 3650",
            &mut report,
        );
        validate_size(
            retention.reference_size_bytes,
            "statistics.retention.reference_size_bytes",
            &mut report,
        );
        let mut names = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut ips = BTreeSet::new();
        for (index, client) in self.clients.iter().enumerate() {
            let path = format!("clients[{index}]");
            validate_name(&client.name, format!("{path}.name"), &mut report);
            for (unique, field) in [
                (names.insert(&client.name), "name"),
                (ids.insert(&client.client_id), "client_id"),
            ] {
                if !unique {
                    report.push(ConfigError::new(
                        ConfigErrorKind::Duplicate,
                        format!("{path}.{field}"),
                        "duplicate management name or request identity",
                    ));
                }
            }
            check(
                valid_client_id(&client.client_id),
                &format!("{path}.client_id"),
                "client_id must contain 1..=128 URL unreserved ASCII characters",
                &mut report,
            );
            check(
                client.r#match.ips.len() <= MAX_CLIENT_IPS,
                &format!("{path}.match.ips"),
                "at most 256 IP ranges are allowed",
                &mut report,
            );
            for (ip_index, ip) in client.r#match.ips.iter().enumerate() {
                if !ips.insert(*ip) {
                    report.push(ConfigError::new(
                        ConfigErrorKind::Duplicate,
                        format!("{path}.match.ips[{ip_index}]"),
                        "duplicate normalized CIDR",
                    ));
                }
            }
            validate_client_strategy(client.strategy.as_deref(), index, &resources, &mut report);
            if let Some(cache) = &client.cache {
                validate_cache_override(cache, format!("{path}.cache"), &mut report);
            }
            if let Some(ttl) = &client.ttl_override {
                validate_ttl(ttl, format!("{path}.ttl_override"), &mut report);
            }
            validate_ecs(
                client.edns_client_subnet.as_ref(),
                format!("{path}.edns_client_subnet"),
                &mut report,
            );
        }
        report.sort_deterministically();
        if report.is_empty() {
            Ok(())
        } else {
            Err(report)
        }
    }

    /// 解析两级路径并拒绝已知词法碰撞；文件身份、symlink 和 reparse 防护仍由打开文件的 owner 负责。
    pub fn resolve_paths(&self, source: &Path) -> Result<V2Paths, ConfigErrorReport> {
        if !source.is_absolute() {
            return Err(single_error(
                ConfigErrorKind::InvalidValue,
                "source",
                "absolute source path required",
            ));
        }
        let config_dir = source.parent().ok_or_else(|| {
            single_error(
                ConfigErrorKind::InvalidValue,
                "source",
                "configuration filename required",
            )
        })?;
        let work = lexical_normalize(&config_dir.join(&self.work.path));
        let resolve = |path: &Path| lexical_normalize(&work.join(path));
        let paths = V2Paths {
            work: work.clone(),
            statistics: resolve(&self.database.path),
            records: resolve(&self.database.records_path),
            snapshot: resolve(&self.dns.cache.clone().unwrap_or_default().persistence.path),
        };
        let protected = [
            lexical_normalize(source),
            work.join("config.yaml"),
            resolve(&self.logs.path),
        ];
        let mut report = ConfigErrorReport::default();
        for (field, path) in [
            ("database.path", &paths.statistics),
            ("dns.cache.persistence.path", &paths.snapshot),
        ] {
            check(
                !paths_overlap(path, &paths.records),
                field,
                "file overlaps records directory",
                &mut report,
            );
            check(
                !protected.iter().any(|other| paths_overlap(path, other)),
                field,
                "file overlaps protected configuration or log",
                &mut report,
            );
        }
        check(
            !paths_overlap(&paths.statistics, &paths.snapshot),
            "dns.cache.persistence.path",
            "snapshot overlaps statistics database",
            &mut report,
        );
        check(
            !protected
                .iter()
                .any(|path| paths_overlap(path, &paths.records)),
            "database.records_path",
            "records directory overlaps protected configuration or log",
            &mut report,
        );
        if report.is_empty() {
            Ok(paths)
        } else {
            Err(report)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct V2Paths {
    pub work: PathBuf,
    pub statistics: PathBuf,
    pub records: PathBuf,
    pub snapshot: PathBuf,
}

fn reject_null(
    value: &yaml_serde::Value,
    path: &str,
    depth: usize,
) -> Result<(), ConfigErrorReport> {
    use yaml_serde::Value;
    if depth > 64 || matches!(value, Value::Null) {
        return Err(single_error(
            ConfigErrorKind::InvalidValue,
            path,
            "null or excessive nesting is not allowed",
        ));
    }
    match value {
        Value::Sequence(items) => {
            for (index, item) in items.iter().enumerate() {
                reject_null(item, &format!("{path}[{index}]"), depth + 1)?;
            }
        }
        Value::Mapping(items) => {
            for (key, item) in items {
                let field = key
                    .as_str()
                    .filter(|name| {
                        name.len() <= 128
                            && name
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    })
                    .unwrap_or("<field>");
                reject_null(item, &format!("{path}.{field}"), depth + 1)?;
            }
        }
        Value::Tagged(_) => {
            return Err(single_error(
                ConfigErrorKind::InvalidValue,
                path,
                "YAML tags are not supported",
            ));
        }
        _ => {}
    }
    Ok(())
}

fn path_key(path: &Path) -> PathBuf {
    let path = lexical_normalize(path);
    #[cfg(windows)]
    let path = PathBuf::from(path.as_os_str().to_string_lossy().to_lowercase());
    path
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = path_key(left);
    let right = path_key(right);
    // 文件不能占据另一逻辑文件或目录的父路径；不依赖尚未初始化的物理布局。
    left.starts_with(&right) || right.starts_with(&left)
}

fn check(valid: bool, path: &str, message: &str, report: &mut ConfigErrorReport) {
    if !valid {
        report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            path,
            message,
        ));
    }
}

fn validate_size(size: u64, path: &str, report: &mut ConfigErrorReport) {
    check(
        (1..=MAX_SIZE_BYTES).contains(&size),
        path,
        "bytes must be in 1..=1099511627776",
        report,
    );
}

fn single_error(kind: ConfigErrorKind, path: &str, message: &str) -> ConfigErrorReport {
    let mut report = ConfigErrorReport::default();
    report.push(ConfigError::new(kind, path, message));
    report
}

#[cfg(test)]
mod tests;
