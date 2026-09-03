//! Semantic configuration validation and deterministic error reporting.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use ipnet::IpNet;

use super::model::{
    ClientIpSource, ConfigDto, EcsDto, EcsMode, HostsResourceDto, ListenerDto, OutboundDto,
    RuleSetDto, StrategyDto, TlsMode, UpstreamDto, UpstreamMode, is_non_empty_path,
};

/// Stable categories used by callers and tests; messages are deliberately non-sensitive.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConfigErrorKind {
    Parse,
    UnsupportedVersion,
    MissingField,
    InvalidValue,
    UnknownField,
    Duplicate,
    MissingReference,
    WrongReferenceKind,
    Cycle,
    Constraint,
    UnsupportedFeature,
    BindConflict,
    Secret,
    Snapshot,
    Migration,
}

impl ConfigErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::UnsupportedVersion => "unsupported_version",
            Self::MissingField => "missing_field",
            Self::InvalidValue => "invalid_value",
            Self::UnknownField => "unknown_field",
            Self::Duplicate => "duplicate",
            Self::MissingReference => "missing_reference",
            Self::WrongReferenceKind => "wrong_reference_kind",
            Self::Cycle => "cycle",
            Self::Constraint => "constraint",
            Self::UnsupportedFeature => "unsupported_feature",
            Self::BindConflict => "bind_conflict",
            Self::Secret => "secret",
            Self::Snapshot => "snapshot",
            Self::Migration => "migration",
        }
    }
}

impl fmt::Display for ConfigErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One safe, path-oriented configuration diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError {
    pub kind: ConfigErrorKind,
    pub path: String,
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl ConfigError {
    pub fn new(kind: ConfigErrorKind, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            message: message.into(),
            line: None,
            column: None,
        }
    }

    pub fn with_location(mut self, line: usize, column: usize) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}: {}",
            self.kind, self.path, self.message
        )?;
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(formatter, " (line {line}, column {column})")?;
        }
        Ok(())
    }
}

/// A deterministic aggregate of independent errors and migration warnings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigErrorReport {
    pub errors: Vec<ConfigError>,
    pub warnings: Vec<String>,
}

impl ConfigErrorReport {
    pub fn push(&mut self, error: ConfigError) {
        self.errors.push(error);
    }

    pub fn warning(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    pub fn extend(&mut self, other: Self) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn sort_deterministically(&mut self) {
        self.errors.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.message.cmp(&right.message))
        });
        self.warnings.sort();
        self.warnings.dedup();
    }

    pub fn first(&self) -> Option<&ConfigError> {
        self.errors.first()
    }
}

impl fmt::Display for ConfigErrorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index != 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{error}")?;
        }
        if self.errors.is_empty() && !self.warnings.is_empty() {
            formatter.write_str("configuration warnings only")?;
        }
        Ok(())
    }
}

/// Underlying socket protocol used by the bind planner.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BindProtocol {
    Udp,
    Tcp,
}

/// Application-level transport retained alongside the underlying socket protocol.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BindTransport {
    Udp,
    Tcp,
    Doh,
}

/// Stable identity of a DoH endpoint in the resolved listener configuration.
///
/// This is carried by bind entries so runtime consumers do not need to parse
/// the human-readable `owner` label.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DohBindingRef {
    pub listener_id: String,
    pub endpoint_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindEntry {
    pub protocol: BindProtocol,
    pub transport: BindTransport,
    pub doh_binding: Option<DohBindingRef>,
    pub address: IpAddr,
    pub port: u16,
    pub owner: String,
    pub v6_only: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BindPlan {
    pub entries: Vec<BindEntry>,
}

impl BindPlan {
    pub fn sort_deterministically(&mut self) {
        self.entries.sort_by(|left, right| {
            left.protocol
                .cmp(&right.protocol)
                .then_with(|| left.transport.cmp(&right.transport))
                .then_with(|| left.port.cmp(&right.port))
                .then_with(|| left.address.to_string().cmp(&right.address.to_string()))
                .then_with(|| left.owner.cmp(&right.owner))
        });
    }
}

/// Validate all semantic constraints that do not require external resource I/O.
pub fn validate_config(config: &ConfigDto) -> Result<(), ConfigErrorReport> {
    let mut report = ConfigErrorReport::default();
    validate_basic(config, &mut report);
    validate_collections(config, &mut report);
    validate_references(config, &mut report);
    validate_upstream_cycles(config, &mut report);
    if let Err(bind_errors) = build_bind_plan(config) {
        report.extend(bind_errors);
    }
    report.sort_deterministically();
    if report.is_empty() {
        Ok(())
    } else {
        Err(report)
    }
}

fn validate_basic(config: &ConfigDto, report: &mut ConfigErrorReport) {
    if config.version != super::model::CURRENT_CONFIG_VERSION {
        report.push(ConfigError::new(
            ConfigErrorKind::UnsupportedVersion,
            "version",
            "only configuration version 1 is supported",
        ));
    }

    if !is_non_empty_path(&config.work.path) {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            "work.path",
            "path must not be empty",
        ));
    }
    if !is_non_empty_path(&config.work.rules_path) {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            "work.rules_path",
            "path must not be empty",
        ));
    }
    if !is_non_empty_path(&config.database.path) {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            "database.path",
            "database path must not be empty",
        ));
    }
    if !is_non_empty_path(&config.logs.path) {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            "logs.path",
            "log path must not be empty",
        ));
    }

    for (path, is_empty, message) in [
        (
            "listener",
            config.listener.is_empty(),
            "at least one listener is required",
        ),
        (
            "upstreams",
            config.upstreams.is_empty(),
            "at least one upstream is required",
        ),
        (
            "strategy",
            config.strategy.is_empty(),
            "at least one strategy is required",
        ),
    ] {
        if is_empty {
            report.push(ConfigError::new(
                ConfigErrorKind::MissingField,
                path,
                message,
            ));
        }
    }

    if config.webui.enable {
        report.push(ConfigError::new(
            ConfigErrorKind::UnsupportedFeature,
            "webui.enable",
            "webui management server is not available in this build yet",
        ));
    }
    if config.webui.port == 0 {
        report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            "webui.port",
            "port must be between 1 and 65535",
        ));
    }
    match &config.webui.public_origin {
        None if config.webui.enable => report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            "webui.public_origin",
            "public_origin is required when webui is enabled",
        )),
        Some(origin) if !is_valid_public_origin(origin) => report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            "webui.public_origin",
            "public_origin must be an absolute http or https origin without credentials, path, query, or fragment",
        )),
        _ => {}
    }
    let mut users = BTreeSet::new();
    for (index, user) in config.webui.users.iter().enumerate() {
        let path = format!("webui.users[{index}]");
        validate_name(&user.name, format!("{path}.name"), report);
        if !users.insert(user.name.clone()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}.name"),
                "user name is duplicated",
            ));
        }
        if !is_supported_password_hash(&user.password_hash) {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                format!("{path}.password_hash"),
                "password_hash must use a supported one-way hash format",
            ));
        }
    }

    if let Some(cache) = &config.dns.cache {
        if cache.memory.max_size_bytes == 0 {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "dns.cache.memory.max_size_bytes",
                "size must be greater than zero",
            ));
        }
        if !(Duration::from_secs(1)..=Duration::from_secs(300)).contains(&cache.failure_ttl) {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "dns.cache.failure_ttl",
                "failure_ttl must be between 1s and 5m",
            ));
        }
        validate_optimistic(&cache.optimistic, "dns.cache.optimistic", report);
        if cache.persistence.max_size_bytes == 0 {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "dns.cache.persistence.max_size_bytes",
                "size must be greater than zero",
            ));
        }
        if !is_non_empty_path(&cache.persistence.path) {
            report.push(ConfigError::new(
                ConfigErrorKind::MissingField,
                "dns.cache.persistence.path",
                "path must not be empty",
            ));
        }
    }
    if let Some(ttl) = &config.dns.ttl_override {
        validate_ttl(ttl, "dns.ttl_override", report);
    }
    validate_ecs(
        config.dns.edns_client_subnet.as_ref(),
        "dns.edns_client_subnet",
        report,
    );
    if let Some(resolve_log) = &config.dns.resolve_log {
        if resolve_log.eviction_threshold_records == 0
            || resolve_log.eviction_threshold_records >= resolve_log.max_records
        {
            report.push(ConfigError::new(
                ConfigErrorKind::Constraint,
                "dns.resolve_log.eviction_threshold_records",
                "eviction threshold must be greater than zero and less than max_records",
            ));
        }
        if resolve_log.max_records == 0 {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "dns.resolve_log.max_records",
                "max_records must be greater than zero",
            ));
        }
        if resolve_log.max_record_age.is_zero() {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                "dns.resolve_log.max_record_age",
                "duration must be greater than zero",
            ));
        }
    }
}

fn validate_collections(config: &ConfigDto, report: &mut ConfigErrorReport) {
    let mut listeners = BTreeSet::new();
    for (index, listener) in config.listener.iter().enumerate() {
        let path = format!("listener[{index}]");
        let name = match listener {
            ListenerDto::Udp { name, .. }
            | ListenerDto::Tcp { name, .. }
            | ListenerDto::Doh { name, .. } => name,
        };
        validate_name(name, format!("{path}.name"), report);
        if !listeners.insert(name.clone()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}.name"),
                "listener name is duplicated",
            ));
        }
        match listener {
            ListenerDto::Udp {
                addresses,
                port,
                strategy,
                hosts,
                ..
            }
            | ListenerDto::Tcp {
                addresses,
                port,
                strategy,
                hosts,
                ..
            } => {
                validate_addresses(addresses, format!("{path}.addresses"), report);
                validate_port(*port, format!("{path}.port"), report);
                if strategy.trim().is_empty() {
                    report.push(ConfigError::new(
                        ConfigErrorKind::MissingField,
                        format!("{path}.strategy"),
                        "strategy must not be empty",
                    ));
                }
                if hosts.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    report.push(ConfigError::new(
                        ConfigErrorKind::InvalidValue,
                        format!("{path}.hosts"),
                        "hosts reference must not be empty",
                    ));
                }
            }
            ListenerDto::Doh {
                routes, endpoints, ..
            } => {
                if routes.is_empty() {
                    report.push(ConfigError::new(
                        ConfigErrorKind::MissingField,
                        format!("{path}.routes"),
                        "at least one route is required",
                    ));
                }
                if endpoints.is_empty() {
                    report.push(ConfigError::new(
                        ConfigErrorKind::MissingField,
                        format!("{path}.endpoints"),
                        "at least one endpoint is required",
                    ));
                }
                let mut route_paths = BTreeSet::new();
                for (route_index, route) in routes.iter().enumerate() {
                    let route_path = format!("{path}.routes[{route_index}]");
                    if route.path.trim().is_empty() || !route.path.starts_with('/') {
                        report.push(ConfigError::new(
                            ConfigErrorKind::InvalidValue,
                            format!("{route_path}.path"),
                            "route path must be a non-empty absolute HTTP path",
                        ));
                    }
                    if route.path.starts_with("/dns-quer/inner")
                        || route.path.starts_with("/dns-quer/outside")
                    {
                        report.push(ConfigError::new(
                            ConfigErrorKind::Constraint,
                            format!("{route_path}.path"),
                            "path is reserved by the service",
                        ));
                    }
                    if !route_paths.insert(route.path.clone()) {
                        report.push(ConfigError::new(
                            ConfigErrorKind::Duplicate,
                            format!("{route_path}.path"),
                            "route path is duplicated",
                        ));
                    }
                    if route.strategy.trim().is_empty() {
                        report.push(ConfigError::new(
                            ConfigErrorKind::MissingField,
                            format!("{route_path}.strategy"),
                            "strategy reference must not be empty",
                        ));
                    }
                }
                let mut endpoints_seen = BTreeSet::new();
                for (endpoint_index, endpoint) in endpoints.iter().enumerate() {
                    let endpoint_path = format!("{path}.endpoints[{endpoint_index}]");
                    validate_name(&endpoint.name, format!("{endpoint_path}.name"), report);
                    if !endpoints_seen.insert(endpoint.name.clone()) {
                        report.push(ConfigError::new(
                            ConfigErrorKind::Duplicate,
                            format!("{endpoint_path}.name"),
                            "endpoint name is duplicated",
                        ));
                    }
                    validate_addresses(
                        &endpoint.addresses,
                        format!("{endpoint_path}.addresses"),
                        report,
                    );
                    validate_port(endpoint.port, format!("{endpoint_path}.port"), report);
                    match endpoint.tls.mode {
                        TlsMode::Terminate => {
                            validate_required_path(
                                endpoint.tls.certificate_file.as_deref(),
                                format!("{endpoint_path}.tls.certificate_file"),
                                report,
                            );
                            validate_required_path(
                                endpoint.tls.private_key_file.as_deref(),
                                format!("{endpoint_path}.tls.private_key_file"),
                                report,
                            );
                        }
                        TlsMode::External => {
                            if endpoint.tls.certificate_file.is_some() {
                                report.push(ConfigError::new(
                                    ConfigErrorKind::Constraint,
                                    format!("{endpoint_path}.tls.certificate_file"),
                                    "certificate_file is forbidden for external mode",
                                ));
                            }
                            if endpoint.tls.private_key_file.is_some() {
                                report.push(ConfigError::new(
                                    ConfigErrorKind::Constraint,
                                    format!("{endpoint_path}.tls.private_key_file"),
                                    "private_key_file is forbidden for external mode",
                                ));
                            }
                        }
                    }
                    validate_client_ip(&endpoint.client_ip, &endpoint_path, report);
                }
            }
        }
    }

    let mut upstreams = BTreeSet::new();
    for (index, upstream) in config.upstreams.iter().enumerate() {
        let path = format!("upstreams[{index}]");
        let name = upstream_name(upstream);
        validate_name(name, format!("{path}.name"), report);
        if !upstreams.insert(name.to_owned()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}.name"),
                "upstream name is duplicated",
            ));
        }
        validate_upstream(upstream, &path, report);
    }

    let mut strategies = BTreeSet::new();
    for (index, strategy) in config.strategy.iter().enumerate() {
        let path = format!("strategy[{index}]");
        validate_name(&strategy.name, format!("{path}.name"), report);
        if !strategies.insert(strategy.name.clone()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}.name"),
                "strategy name is duplicated",
            ));
        }
        validate_strategy(strategy, &path, report);
    }

    let mut hosts = BTreeSet::new();
    for (index, resource) in config.hosts.iter().enumerate() {
        let path = format!("hosts[{index}]");
        let name = match resource {
            HostsResourceDto::Const { name, .. } | HostsResourceDto::File { name, .. } => name,
        };
        validate_name(name, format!("{path}.name"), report);
        if !hosts.insert(name.clone()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}.name"),
                "hosts resource name is duplicated",
            ));
        }
        validate_hosts_resource(resource, &path, report);
    }

    let mut outbounds = BTreeSet::new();
    for (index, outbound) in config.outbound.iter().enumerate() {
        let path = format!("outbound[{index}]");
        validate_name(&outbound.name, format!("{path}.name"), report);
        if !outbounds.insert(outbound.name.clone()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}.name"),
                "outbound name is duplicated",
            ));
        }
        validate_outbound(outbound, &path, report);
    }

    let mut rule_sets = BTreeSet::new();
    for (index, rule_set) in config.rule_set.iter().enumerate() {
        let path = format!("rule_set[{index}]");
        let name = rule_set_name(rule_set);
        validate_name(name, format!("{path}.name"), report);
        if !rule_sets.insert(name.to_owned()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}.name"),
                "rule_set name is duplicated",
            ));
        }
        validate_rule_set(rule_set, &path, report);
    }

    let mut clients = BTreeSet::new();
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut cidrs: BTreeMap<IpNet, String> = BTreeMap::new();
    for (index, client) in config.clients.iter().enumerate() {
        let path = format!("clients[{index}]");
        validate_name(&client.name, format!("{path}.name"), report);
        if !clients.insert(client.name.clone()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}.name"),
                "client name is duplicated",
            ));
        }
        if client.r#match.ids.is_empty() && client.r#match.ips.is_empty() {
            report.push(ConfigError::new(
                ConfigErrorKind::Constraint,
                format!("{path}.match"),
                "at least one id or IP range is required",
            ));
        }
        for (id_index, id) in client.r#match.ids.iter().enumerate() {
            if id.trim().is_empty() || id.chars().any(char::is_control) {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.match.ids[{id_index}]"),
                    "client id must be non-empty and printable",
                ));
            }
            if let Some(previous) = ids.insert(id.clone(), client.name.clone()) {
                report.push(ConfigError::new(
                    ConfigErrorKind::Duplicate,
                    format!("{path}.match.ids[{id_index}]"),
                    format!("client id conflicts with `{previous}`"),
                ));
            }
        }
        for (ip_index, ip) in client.r#match.ips.iter().enumerate() {
            if let Some(previous) = cidrs.insert(*ip, client.name.clone()) {
                report.push(ConfigError::new(
                    ConfigErrorKind::Duplicate,
                    format!("{path}.match.ips[{ip_index}]"),
                    format!("client CIDR conflicts with `{previous}`"),
                ));
            }
        }
        if let Some(strategy) = &client.strategy
            && strategy.trim().is_empty()
        {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                format!("{path}.strategy"),
                "strategy reference must not be empty",
            ));
        }
        if let Some(cache) = &client.cache {
            validate_cache_override(cache, format!("{path}.cache"), report);
        }
        if let Some(ttl) = &client.ttl_override {
            validate_ttl(ttl, format!("{path}.ttl_override"), report);
        }
        validate_ecs(
            client.edns_client_subnet.as_ref(),
            format!("{path}.edns_client_subnet"),
            report,
        );
    }
}

fn validate_strategy(strategy: &StrategyDto, path: &str, report: &mut ConfigErrorReport) {
    if strategy.rules.is_empty() {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            format!("{path}.rules"),
            "at least one rule is required",
        ));
    }
    if strategy.default_upstream.trim().is_empty() {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            format!("{path}.default_upstream"),
            "default upstream must not be empty",
        ));
    }
    if let Some(cache) = &strategy.cache {
        validate_cache_override(cache, format!("{path}.cache"), report);
    }
    if let Some(ttl) = &strategy.ttl_override {
        validate_ttl(ttl, format!("{path}.ttl_override"), report);
    }
    validate_ecs(
        strategy.edns_client_subnet.as_ref(),
        format!("{path}.edns_client_subnet"),
        report,
    );
    for (index, rule) in strategy.rules.iter().enumerate() {
        let rule_path = format!("{path}.rules[{index}]");
        let has_rule_set = rule
            .rule_set
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_hosts = rule
            .hosts
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty());
        if has_rule_set == has_hosts {
            report.push(ConfigError::new(
                ConfigErrorKind::Constraint,
                &rule_path,
                "exactly one of rule_set or hosts is required",
            ));
        }
        if has_rule_set
            && rule
                .upstream
                .as_ref()
                .is_none_or(|value| value.trim().is_empty())
        {
            report.push(ConfigError::new(
                ConfigErrorKind::MissingField,
                format!("{rule_path}.upstream"),
                "rule_set rules require an upstream",
            ));
        }
        if has_hosts && rule.upstream.is_some() {
            report.push(ConfigError::new(
                ConfigErrorKind::Constraint,
                format!("{rule_path}.upstream"),
                "hosts rules must not specify an upstream",
            ));
        }
        validate_ecs(
            rule.edns_client_subnet.as_ref(),
            format!("{rule_path}.edns_client_subnet"),
            report,
        );
    }
}

fn validate_upstream(upstream: &UpstreamDto, path: &str, report: &mut ConfigErrorReport) {
    match upstream {
        UpstreamDto::Hosts { format, hosts, .. } => {
            if !matches!(format.as_str(), "json" | "hosts") {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.format"),
                    "hosts upstream format must be json or hosts",
                ));
            }
            if hosts.trim().is_empty() {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.hosts"),
                    "inline hosts content must not be empty",
                ));
            }
        }
        UpstreamDto::Doh {
            address,
            bootstrap,
            connect_ip,
            edns_client_subnet,
            ..
        } => {
            if !matches!(address.scheme(), "https" | "http") {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.address"),
                    "upstream address must use http or https",
                ));
            }
            if address.host_str().is_none() {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.address"),
                    "upstream URL must include a host",
                ));
            }
            if address.username() != ""
                || address.password().is_some()
                || address.fragment().is_some()
            {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.address"),
                    "upstream URL must not contain credentials or a fragment",
                ));
            }
            if bootstrap.is_some() && connect_ip.is_some() {
                report.push(ConfigError::new(
                    ConfigErrorKind::Constraint,
                    path,
                    "bootstrap and connect_ip are mutually exclusive",
                ));
            }
            validate_ecs(
                edns_client_subnet.as_ref(),
                format!("{path}.edns_client_subnet"),
                report,
            );
        }
        UpstreamDto::Group {
            upstreams,
            upstream_mode,
            timeout,
            fallbacks,
            fallback_upstream_mode,
            fallback_timeout,
            ..
        } => {
            validate_group_members(
                upstreams,
                upstream_mode,
                format!("{path}.upstreams"),
                report,
            );
            if timeout.is_zero() {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.timeout"),
                    "timeout must be greater than zero",
                ));
            }
            if let Some(fallbacks) = fallbacks {
                if fallbacks.is_empty() {
                    report.push(ConfigError::new(
                        ConfigErrorKind::Constraint,
                        format!("{path}.fallbacks"),
                        "fallbacks must not be empty when provided",
                    ));
                }
                if fallback_upstream_mode.is_none() {
                    report.push(ConfigError::new(
                        ConfigErrorKind::MissingField,
                        format!("{path}.fallback_upstream_mode"),
                        "fallback mode is required when fallbacks are provided",
                    ));
                }
                if fallback_timeout.is_none_or(|value| value.is_zero()) {
                    report.push(ConfigError::new(
                        ConfigErrorKind::MissingField,
                        format!("{path}.fallback_timeout"),
                        "fallback timeout is required and must be positive",
                    ));
                }
                if let Some(mode) = fallback_upstream_mode {
                    validate_group_members(fallbacks, mode, format!("{path}.fallbacks"), report);
                }
            } else if fallback_upstream_mode.is_some() || fallback_timeout.is_some() {
                report.push(ConfigError::new(
                    ConfigErrorKind::Constraint,
                    path,
                    "fallback mode and timeout require fallbacks",
                ));
            }
        }
    }
}

fn validate_group_members(
    members: &[super::model::UpstreamMemberDto],
    mode: &UpstreamMode,
    path: String,
    report: &mut ConfigErrorReport,
) {
    if members.is_empty() {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            path.clone(),
            "at least one group member is required",
        ));
    }
    let mut names = BTreeSet::new();
    for (index, member) in members.iter().enumerate() {
        let member_path = format!("{path}[{index}]");
        if member.name.trim().is_empty() {
            report.push(ConfigError::new(
                ConfigErrorKind::MissingField,
                format!("{member_path}.name"),
                "member name must not be empty",
            ));
        }
        if !names.insert(member.name.clone()) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{member_path}.name"),
                "group member is duplicated",
            ));
        }
        if member.weight == 0 {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                format!("{member_path}.weight"),
                "weight must be positive",
            ));
        }
        if matches!(mode, UpstreamMode::Parallel | UpstreamMode::Failover) && member.weight != 1 {
            report.push(ConfigError::new(
                ConfigErrorKind::Constraint,
                format!("{member_path}.weight"),
                "parallel and failover members must use weight 1",
            ));
        }
    }
}

fn validate_hosts_resource(
    resource: &HostsResourceDto,
    path: &str,
    report: &mut ConfigErrorReport,
) {
    match resource {
        HostsResourceDto::Const { hosts, .. } => {
            if hosts.trim().is_empty() {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.hosts"),
                    "inline hosts content must not be empty",
                ));
            }
        }
        HostsResourceDto::File {
            path: file_path,
            auto_update,
            update_interval,
            ..
        } => {
            if !is_non_empty_path(file_path) {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.path"),
                    "file path must not be empty",
                ));
            }
            if *auto_update && update_interval.is_none_or(|value| value.is_zero()) {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.update_interval"),
                    "update_interval is required when auto_update is true",
                ));
            }
        }
    }
}

fn validate_outbound(outbound: &OutboundDto, path: &str, report: &mut ConfigErrorReport) {
    let has_env = outbound
        .proxy_url
        .env
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_file = outbound
        .proxy_url
        .file
        .as_ref()
        .is_some_and(|value| !value.as_os_str().is_empty());
    if has_env == has_file {
        report.push(ConfigError::new(
            ConfigErrorKind::Secret,
            format!("{path}.proxy_url"),
            "exactly one of env or file is required",
        ));
    }
    if outbound
        .proxy_url
        .env
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        report.push(ConfigError::new(
            ConfigErrorKind::Secret,
            format!("{path}.proxy_url.env"),
            "environment variable name must not be empty",
        ));
    }
    if let Some(name) = &outbound.proxy_url.env
        && !is_valid_env_name(name)
    {
        report.push(ConfigError::new(
            ConfigErrorKind::Secret,
            format!("{path}.proxy_url.env"),
            "environment variable name contains unsupported characters",
        ));
    }
    if let Some(file) = &outbound.proxy_url.file
        && !is_non_empty_path(file)
    {
        report.push(ConfigError::new(
            ConfigErrorKind::Secret,
            format!("{path}.proxy_url.file"),
            "secret file path must not be empty",
        ));
    }
}

fn validate_rule_set(resource: &RuleSetDto, path: &str, report: &mut ConfigErrorReport) {
    match resource {
        RuleSetDto::Const { rule, .. } => {
            if rule.trim().is_empty() {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.rule"),
                    "inline rule content must not be empty",
                ));
            }
        }
        RuleSetDto::File {
            path: file_path,
            auto_update,
            update_interval,
            ..
        } => {
            if !is_non_empty_path(file_path) {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.path"),
                    "file path must not be empty",
                ));
            }
            if *auto_update && update_interval.is_none_or(|value| value.is_zero()) {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.update_interval"),
                    "update_interval is required when auto_update is true",
                ));
            }
        }
        RuleSetDto::Remote {
            url,
            proxy: _,
            auto_update,
            update_interval,
            ..
        } => {
            if !matches!(url.scheme(), "http" | "https") {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.url"),
                    "remote URL must use http or https",
                ));
            }
            if url.host_str().is_none() {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.url"),
                    "remote URL must include a host",
                ));
            }
            if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
                report.push(ConfigError::new(
                    ConfigErrorKind::InvalidValue,
                    format!("{path}.url"),
                    "remote URL must not contain credentials or a fragment",
                ));
            }
            if *auto_update && update_interval.is_none_or(|value| value.is_zero()) {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.update_interval"),
                    "update_interval is required when auto_update is true",
                ));
            }
        }
    }
}

fn validate_client_ip(
    client_ip: &super::model::ClientIpDto,
    endpoint_path: &str,
    report: &mut ConfigErrorReport,
) {
    let path = format!("{endpoint_path}.client_ip");
    match client_ip.source {
        ClientIpSource::Peer => {
            if client_ip.header.is_some()
                || client_ip.trusted_proxies.is_some()
                || client_ip.on_missing.is_some()
                || client_ip.on_invalid.is_some()
            {
                report.push(ConfigError::new(
                    ConfigErrorKind::Constraint,
                    path,
                    "peer source must not configure proxy header options",
                ));
            }
        }
        ClientIpSource::ForwardedHeader => {
            if client_ip.header.is_none() {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.header"),
                    "header is required for forwarded_header source",
                ));
            }
            if client_ip.trusted_proxies.as_ref().is_none_or(Vec::is_empty) {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.trusted_proxies"),
                    "trusted_proxies is required for forwarded_header source",
                ));
            }
        }
        ClientIpSource::ProxyProtocol => {
            if client_ip.trusted_proxies.as_ref().is_none_or(Vec::is_empty) {
                report.push(ConfigError::new(
                    ConfigErrorKind::MissingField,
                    format!("{path}.trusted_proxies"),
                    "trusted_proxies is required for proxy_protocol source",
                ));
            }
            if client_ip.header.is_some()
                || client_ip.on_missing.is_some()
                || client_ip.on_invalid.is_some()
            {
                report.push(ConfigError::new(
                    ConfigErrorKind::Constraint,
                    path,
                    "proxy_protocol source does not accept forwarded header options",
                ));
            }
        }
    }
}

fn validate_cache_override(
    cache: &super::model::CacheOverrideDto,
    path: String,
    report: &mut ConfigErrorReport,
) {
    if cache.enabled.is_none() {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            format!("{path}.enabled"),
            "enabled is required when a cache override is present",
        ));
    }
    if let Some(optimistic) = &cache.optimistic {
        validate_optimistic(optimistic, format!("{path}.optimistic"), report);
    }
}

fn validate_optimistic(
    optimistic: &super::model::OptimisticDto,
    path: impl Into<String>,
    report: &mut ConfigErrorReport,
) {
    let path = path.into();
    if optimistic.answer_ttl.is_zero() {
        report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            format!("{path}.answer_ttl"),
            "duration must be greater than zero",
        ));
    }
    if optimistic.max_age.is_zero() {
        report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            format!("{path}.max_age"),
            "duration must be greater than zero",
        ));
    }
}

fn validate_ttl(
    ttl: &super::model::TtlOverrideDto,
    path: impl Into<String>,
    report: &mut ConfigErrorReport,
) {
    let path = path.into();
    if let (Some(min), Some(max)) = (ttl.min, ttl.max)
        && !max.is_zero()
        && !min.is_zero()
        && min > max
    {
        report.push(ConfigError::new(
            ConfigErrorKind::Constraint,
            path,
            "min TTL must not exceed max TTL",
        ));
    }
}

fn validate_ecs(ecs: Option<&EcsDto>, path: impl Into<String>, report: &mut ConfigErrorReport) {
    let path = path.into();
    if let Some(ecs) = ecs {
        if matches!(ecs.mode, EcsMode::Custom) && ecs.custom_ip.is_none() {
            report.push(ConfigError::new(
                ConfigErrorKind::MissingField,
                format!("{path}.custom_ip"),
                "custom_ip is required for custom mode",
            ));
        }
        if !matches!(ecs.mode, EcsMode::Custom) && ecs.custom_ip.is_some() {
            report.push(ConfigError::new(
                ConfigErrorKind::Constraint,
                format!("{path}.custom_ip"),
                "custom_ip is only allowed for custom mode",
            ));
        }
    }
}

fn validate_references(config: &ConfigDto, report: &mut ConfigErrorReport) {
    let strategies: BTreeSet<&str> = config
        .strategy
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    let upstreams: BTreeSet<&str> = config.upstreams.iter().map(upstream_name).collect();
    let hosts: BTreeSet<&str> = config
        .hosts
        .iter()
        .map(|item| match item {
            HostsResourceDto::Const { name, .. } | HostsResourceDto::File { name, .. } => {
                name.as_str()
            }
        })
        .collect();
    let rule_sets: BTreeSet<&str> = config.rule_set.iter().map(rule_set_name).collect();
    let outbounds: BTreeSet<&str> = config
        .outbound
        .iter()
        .map(|item| item.name.as_str())
        .collect();

    let all_names: BTreeSet<&str> = strategies
        .iter()
        .chain(upstreams.iter())
        .chain(hosts.iter())
        .chain(rule_sets.iter())
        .chain(outbounds.iter())
        .copied()
        .collect();

    let check = |value: &str,
                 path: String,
                 expected: &'static str,
                 target: &BTreeSet<&str>,
                 report: &mut ConfigErrorReport| {
        if target.contains(value) {
            return;
        }
        let kind = if all_names.contains(value) {
            ConfigErrorKind::WrongReferenceKind
        } else {
            ConfigErrorKind::MissingReference
        };
        report.push(ConfigError::new(
            kind,
            path,
            format!("reference must point to {expected}"),
        ));
    };

    for (index, listener) in config.listener.iter().enumerate() {
        let path = format!("listener[{index}]");
        match listener {
            ListenerDto::Udp {
                strategy,
                hosts: local_hosts,
                ..
            }
            | ListenerDto::Tcp {
                strategy,
                hosts: local_hosts,
                ..
            } => {
                check(
                    strategy,
                    format!("{path}.strategy"),
                    "strategy",
                    &strategies,
                    report,
                );
                if let Some(hosts_ref) = local_hosts {
                    check(hosts_ref, format!("{path}.hosts"), "hosts", &hosts, report);
                }
            }
            ListenerDto::Doh { routes, .. } => {
                for (route_index, route) in routes.iter().enumerate() {
                    check(
                        &route.strategy,
                        format!("{path}.routes[{route_index}].strategy"),
                        "strategy",
                        &strategies,
                        report,
                    );
                }
            }
        }
    }
    for (index, upstream) in config.upstreams.iter().enumerate() {
        let path = format!("upstreams[{index}]");
        match upstream {
            UpstreamDto::Doh {
                bootstrap, proxy, ..
            } => {
                if let Some(value) = bootstrap {
                    check(
                        value,
                        format!("{path}.bootstrap"),
                        "upstream",
                        &upstreams,
                        report,
                    );
                }
                if let Some(value) = proxy {
                    check(
                        value,
                        format!("{path}.proxy"),
                        "outbound",
                        &outbounds,
                        report,
                    );
                }
            }
            UpstreamDto::Group {
                upstreams: members,
                fallbacks,
                ..
            } => {
                for (member_index, member) in members.iter().enumerate() {
                    check(
                        &member.name,
                        format!("{path}.upstreams[{member_index}].name"),
                        "upstream",
                        &upstreams,
                        report,
                    );
                }
                if let Some(fallbacks) = fallbacks {
                    for (member_index, member) in fallbacks.iter().enumerate() {
                        check(
                            &member.name,
                            format!("{path}.fallbacks[{member_index}].name"),
                            "upstream",
                            &upstreams,
                            report,
                        );
                    }
                }
            }
            UpstreamDto::Hosts { .. } => {}
        }
    }
    for (index, strategy) in config.strategy.iter().enumerate() {
        let path = format!("strategy[{index}]");
        check(
            &strategy.default_upstream,
            format!("{path}.default_upstream"),
            "upstream",
            &upstreams,
            report,
        );
        for (rule_index, rule) in strategy.rules.iter().enumerate() {
            let rule_path = format!("{path}.rules[{rule_index}]");
            if let Some(value) = &rule.hosts {
                check(value, format!("{rule_path}.hosts"), "hosts", &hosts, report);
            }
            if let Some(value) = &rule.rule_set {
                check_rule_set_selector(value, &rule_path, &rule_sets, report);
            }
            if let Some(value) = &rule.upstream {
                check(
                    value,
                    format!("{rule_path}.upstream"),
                    "upstream",
                    &upstreams,
                    report,
                );
            }
        }
    }
    for (index, rule_set) in config.rule_set.iter().enumerate() {
        if let RuleSetDto::Remote {
            proxy: Some(proxy), ..
        } = rule_set
        {
            check(
                proxy,
                format!("rule_set[{index}].proxy"),
                "outbound",
                &outbounds,
                report,
            );
        }
    }
    for (index, client) in config.clients.iter().enumerate() {
        if let Some(strategy) = &client.strategy {
            check(
                strategy,
                format!("clients[{index}].strategy"),
                "strategy",
                &strategies,
                report,
            );
        }
    }
}

fn check_rule_set_selector(
    value: &str,
    path: &str,
    rule_sets: &BTreeSet<&str>,
    report: &mut ConfigErrorReport,
) {
    if rule_sets.contains(value) {
        return;
    }
    if let Some((base, selector)) = value.split_once(':') {
        if !rule_sets.contains(base) {
            report.push(ConfigError::new(
                ConfigErrorKind::MissingReference,
                format!("{path}.rule_set"),
                "rule_set selector base does not exist",
            ));
        }
        if selector.is_empty()
            || !selector.is_ascii()
            || selector != selector.to_ascii_lowercase()
            || !selector.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            report.push(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                format!("{path}.rule_set"),
                "rule_set selector must be non-empty lowercase ASCII",
            ));
        }
    } else {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingReference,
            format!("{path}.rule_set"),
            "rule_set reference does not exist",
        ));
    }
}

fn validate_upstream_cycles(config: &ConfigDto, report: &mut ConfigErrorReport) {
    let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for upstream in &config.upstreams {
        let name = upstream_name(upstream).to_owned();
        let edges = match upstream {
            UpstreamDto::Doh {
                bootstrap: Some(value),
                ..
            } => vec![value.clone()],
            UpstreamDto::Group {
                upstreams,
                fallbacks,
                ..
            } => upstreams
                .iter()
                .chain(fallbacks.iter().flatten())
                .map(|member| member.name.clone())
                .collect(),
            _ => Vec::new(),
        };
        graph.insert(name, edges);
    }
    for edges in graph.values_mut() {
        edges.sort();
        edges.dedup();
    }

    fn visit(
        node: &str,
        graph: &BTreeMap<String, Vec<String>>,
        states: &mut BTreeMap<String, u8>,
        stack: &mut Vec<String>,
        report: &mut ConfigErrorReport,
    ) {
        match states.get(node).copied().unwrap_or(0) {
            1 => {
                let start = stack.iter().position(|value| value == node).unwrap_or(0);
                let mut cycle = stack[start..].to_vec();
                cycle.push(node.to_owned());
                report.push(ConfigError::new(
                    ConfigErrorKind::Cycle,
                    format!("upstreams.{}", node),
                    format!("upstream cycle: {}", cycle.join(" -> ")),
                ));
                return;
            }
            2 => return,
            _ => {}
        }
        states.insert(node.to_owned(), 1);
        stack.push(node.to_owned());
        if let Some(edges) = graph.get(node) {
            for edge in edges {
                if graph.contains_key(edge) {
                    visit(edge, graph, states, stack, report);
                }
            }
        }
        stack.pop();
        states.insert(node.to_owned(), 2);
    }

    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    for node in graph.keys() {
        visit(node, &graph, &mut states, &mut stack, report);
    }
}

/// Build and validate the expanded socket bind plan.
pub fn build_bind_plan(config: &ConfigDto) -> Result<BindPlan, ConfigErrorReport> {
    let mut report = ConfigErrorReport::default();
    let mut plan = BindPlan::default();
    for (index, listener) in config.listener.iter().enumerate() {
        match listener {
            ListenerDto::Udp {
                name,
                addresses,
                port,
                ..
            } => {
                push_bind_entries(
                    &mut plan,
                    BindProtocol::Udp,
                    BindTransport::Udp,
                    None,
                    name,
                    addresses,
                    *port,
                );
            }
            ListenerDto::Tcp {
                name,
                addresses,
                port,
                ..
            } => {
                push_bind_entries(
                    &mut plan,
                    BindProtocol::Tcp,
                    BindTransport::Tcp,
                    None,
                    name,
                    addresses,
                    *port,
                );
            }
            ListenerDto::Doh {
                name, endpoints, ..
            } => {
                for endpoint in endpoints {
                    let doh_binding = DohBindingRef {
                        listener_id: name.clone(),
                        endpoint_id: endpoint.name.clone(),
                    };
                    push_bind_entries(
                        &mut plan,
                        BindProtocol::Tcp,
                        BindTransport::Doh,
                        Some(doh_binding),
                        &format!("listener[{index}].{}.{}", name, endpoint.name),
                        &endpoint.addresses,
                        endpoint.port,
                    );
                }
            }
        }
    }
    for left_index in 0..plan.entries.len() {
        for right_index in (left_index + 1)..plan.entries.len() {
            let left = &plan.entries[left_index];
            let right = &plan.entries[right_index];
            if left.protocol == right.protocol
                && left.port == right.port
                && addresses_overlap(left.address, right.address)
            {
                report.push(ConfigError::new(
                    ConfigErrorKind::BindConflict,
                    format!("bind.{}.{}", left.owner, left.port),
                    "expanded listener addresses conflict",
                ));
            }
        }
    }
    if config.webui.enable {
        for entry in &plan.entries {
            if entry.protocol == BindProtocol::Tcp
                && entry.port == config.webui.port
                && addresses_overlap(entry.address, config.webui.address)
            {
                report.push(ConfigError::new(
                    ConfigErrorKind::BindConflict,
                    format!("bind.webui.{}", config.webui.port),
                    "management endpoint conflicts with a DNS TCP endpoint",
                ));
            }
        }
    }
    report.sort_deterministically();
    if report.is_empty() {
        plan.sort_deterministically();
        Ok(plan)
    } else {
        Err(report)
    }
}

fn push_bind_entries(
    plan: &mut BindPlan,
    protocol: BindProtocol,
    transport: BindTransport,
    doh_binding: Option<DohBindingRef>,
    owner: &str,
    addresses: &[IpAddr],
    port: u16,
) {
    for address in addresses {
        plan.entries.push(BindEntry {
            protocol,
            transport,
            doh_binding: doh_binding.clone(),
            address: *address,
            port,
            owner: owner.to_owned(),
            v6_only: address.is_ipv6(),
        });
    }
}

fn addresses_overlap(left: IpAddr, right: IpAddr) -> bool {
    match (left, right) {
        (IpAddr::V4(left), IpAddr::V4(right)) => {
            left == right || left.is_unspecified() || right.is_unspecified()
        }
        (IpAddr::V6(left), IpAddr::V6(right)) => {
            left == right || left.is_unspecified() || right.is_unspecified()
        }
        _ => false,
    }
}

fn validate_addresses(addresses: &[IpAddr], path: String, report: &mut ConfigErrorReport) {
    if addresses.is_empty() {
        report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            path.clone(),
            "at least one address is required",
        ));
    }
    let mut seen = BTreeSet::new();
    for (index, address) in addresses.iter().enumerate() {
        if !seen.insert(*address) {
            report.push(ConfigError::new(
                ConfigErrorKind::Duplicate,
                format!("{path}[{index}]"),
                "address is duplicated",
            ));
        }
    }
}

fn validate_port(port: u16, path: String, report: &mut ConfigErrorReport) {
    if port == 0 {
        report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            path,
            "port must be between 1 and 65535",
        ));
    }
}

fn validate_required_path(
    path: Option<&std::path::Path>,
    field_path: String,
    report: &mut ConfigErrorReport,
) {
    match path {
        None => report.push(ConfigError::new(
            ConfigErrorKind::MissingField,
            field_path,
            "path is required",
        )),
        Some(path) if !is_non_empty_path(path) => report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            field_path,
            "path must not be empty",
        )),
        Some(_) => {}
    }
}

fn validate_name(name: &str, path: String, report: &mut ConfigErrorReport) {
    if name.is_empty() || name.len() > 128 {
        report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            path,
            "name must contain 1..=128 characters",
        ));
        return;
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!'))
    {
        report.push(ConfigError::new(
            ConfigErrorKind::InvalidValue,
            path,
            "name contains an unsupported character",
        ));
    }
}

fn is_supported_password_hash(value: &str) -> bool {
    let bcrypt =
        (value.starts_with("$2a$") || value.starts_with("$2b$") || value.starts_with("$2y$"))
            && value.len() == 60;
    bcrypt || (value.starts_with("$argon2id$") && value.len() >= 20)
}

fn is_valid_public_origin(origin: &url::Url) -> bool {
    matches!(origin.scheme(), "http" | "https")
        && origin.host().is_some()
        && origin.username().is_empty()
        && origin.password().is_none()
        && origin.path() == "/"
        && origin.query().is_none()
        && origin.fragment().is_none()
}

fn is_valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(
        bytes.next(),
        Some(b'_') | Some(b'a'..=b'z') | Some(b'A'..=b'Z')
    ) && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn upstream_name(value: &UpstreamDto) -> &str {
    match value {
        UpstreamDto::Hosts { name, .. }
        | UpstreamDto::Doh { name, .. }
        | UpstreamDto::Group { name, .. } => name,
    }
}

fn rule_set_name(value: &RuleSetDto) -> &str {
    match value {
        RuleSetDto::Const { name, .. }
        | RuleSetDto::File { name, .. }
        | RuleSetDto::Remote { name, .. } => name,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::config::{ConfigLoader, LoadOptions};

    use super::{
        BindProtocol, BindTransport, ConfigErrorKind, ConfigErrorReport, DohBindingRef,
        addresses_overlap,
    };

    #[test]
    fn wildcard_overlap_is_family_local() {
        assert!(addresses_overlap(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        ));
        assert!(!addresses_overlap(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            "::1".parse().unwrap()
        ));
    }

    #[test]
    fn error_categories_are_stable_strings() {
        assert_eq!(ConfigErrorKind::BindConflict.as_str(), "bind_conflict");
        assert_eq!(BindProtocol::Tcp, BindProtocol::Tcp);
        assert_eq!(BindTransport::Doh, BindTransport::Doh);
    }

    #[test]
    fn bind_plan_retains_doh_transport_without_confusing_tcp() {
        let (source, _) = crate::config::test_support::portable_example();
        let output = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("repository example must remain a valid configuration");

        assert!(
            output
                .resolved
                .bind_plan
                .entries
                .iter()
                .any(|entry| entry.transport == BindTransport::Doh
                    && entry.protocol == BindProtocol::Tcp)
        );
        assert!(output.resolved.bind_plan.entries.iter().any(|entry| {
            entry.doh_binding
                == Some(DohBindingRef {
                    listener_id: "doh".to_owned(),
                    endpoint_id: "direct".to_owned(),
                })
        }));
        assert!(
            output.resolved.bind_plan.entries.iter().all(|entry| {
                entry.transport == BindTransport::Doh || entry.doh_binding.is_none()
            })
        );
        assert!(
            output.resolved.bind_plan.entries.iter().all(|entry| {
                entry.transport != BindTransport::Doh || entry.doh_binding.is_some()
            })
        );
        assert!(
            output
                .resolved
                .bind_plan
                .entries
                .iter()
                .any(|entry| entry.transport == BindTransport::Udp
                    && entry.protocol == BindProtocol::Udp)
        );
    }

    #[test]
    fn management_endpoint_conflicts_with_tcp_but_not_udp() {
        let (source, _) = crate::config::test_support::portable_example();
        let mut config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("repository example must remain a valid configuration")
            .config;
        config.webui.enable = true;
        config.webui.public_origin = Some("http://127.0.0.1:53".parse().unwrap());
        config.webui.address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        config.webui.port = 53;

        let report = super::build_bind_plan(&config).unwrap_err();
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.kind == ConfigErrorKind::BindConflict)
        );

        config
            .listener
            .retain(|listener| !matches!(listener, crate::config::model::ListenerDto::Tcp { .. }));
        assert!(super::build_bind_plan(&config).is_ok());
    }

    #[test]
    fn webui_public_origin_accepts_http_and_rejects_missing_or_path() {
        let (source, _) = crate::config::test_support::portable_example();
        let mut config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(&source)
            .expect("repository example must remain a valid configuration")
            .config;
        config.webui.enable = true;

        config.webui.public_origin = Some("http://127.0.0.1:8080".parse().unwrap());
        let mut report = ConfigErrorReport::default();
        super::validate_basic(&config, &mut report);
        assert!(
            !report
                .errors
                .iter()
                .any(|error| error.path == "webui.public_origin")
        );

        config.webui.public_origin = None;
        let mut report = ConfigErrorReport::default();
        super::validate_basic(&config, &mut report);
        assert!(report.errors.iter().any(|error| {
            error.kind == ConfigErrorKind::MissingField && error.path == "webui.public_origin"
        }));

        config.webui.public_origin = Some("https://dns.example.com/admin".parse().unwrap());
        let mut report = ConfigErrorReport::default();
        super::validate_basic(&config, &mut report);
        assert!(report.errors.iter().any(|error| {
            error.kind == ConfigErrorKind::InvalidValue && error.path == "webui.public_origin"
        }));
    }
}
