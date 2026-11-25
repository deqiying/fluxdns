//! v2 配置状态的白名单投影；正文来自内存配置文档，effective/runtime 来自同代 Runtime。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::contract::{
    CacheSnapshotStatus, ConfigModule, ConfigRead, ConfigState, DecimalU64, DiskState, DiskStatus,
    EffectiveField, ErrorCode, ListenerBinding, ModuleRuntime, ModuleSource, ResourceCondition,
    ResourceReference, RestartComponent, Revision, ScalarValue, SnapshotCondition,
    SystemConfigRead, Transport, ValueOrigin,
};
use crate::cache::{CacheSnapshotCondition, CacheSnapshotFailure};
use crate::config::contract::ConfigV2;
use crate::config::disk::DiskStatus as StoreDiskStatus;
use crate::config::document::ConfigDocument;
use crate::config::model::{HostsResourceDto, RuleSetDto, format_duration};
use crate::config::resolve::{ResolvedConfig, ResolvedUpstream, ValueSource};
use crate::config::store::ConfigStore;
use crate::resource::ResourceStaleStatus;
use crate::runtime::RuntimeCoordinator;

/// 当前配置状态；不读盘，磁盘状态来自最近一次观测。
pub(crate) fn configuration_state(store: &ConfigStore) -> Result<ConfigState, ErrorCode> {
    let document = store.current();
    document_state(store, &document)
}

/// 以同一份文档投影状态，避免模块与系统读取跨代。
pub(super) fn document_state(
    store: &ConfigStore,
    document: &ConfigDocument,
) -> Result<ConfigState, ErrorCode> {
    Ok(ConfigState {
        config_digest: Revision::try_from(document.digest.clone())
            .map_err(|_| ErrorCode::ServiceUnavailable)?,
        // RuntimeRevision 的 u64 只编码为十进制字符串，不输出不安全 JSON number。
        runtime_revision: DecimalU64::from(document.runtime_revision),
        disk: disk_state(store.disk_status(&document.digest)),
    })
}

pub(super) fn disk_state(status: StoreDiskStatus) -> DiskState {
    DiskState {
        status: match status {
            StoreDiskStatus::InSync => DiskStatus::InSync,
            StoreDiskStatus::Modified => DiskStatus::Modified,
            StoreDiskStatus::Missing => DiskStatus::Missing,
            StoreDiskStatus::Unreadable => DiskStatus::Unreadable,
        },
    }
}

/// 只返回请求模块的源配置、有效值、出站引用及运行态。
///
/// 文档与 Runtime 不同代（切换与发布之间的短窗口）时返回可重试的 `SERVICE_UNAVAILABLE`。
pub(crate) fn configuration_module(
    store: &ConfigStore,
    coordinator: &RuntimeCoordinator,
    module: ConfigModule,
) -> Result<ConfigRead, ErrorCode> {
    let document = store.current();
    let runtime = coordinator.load();
    if document.runtime_revision != runtime.revision().0 {
        return Err(ErrorCode::ServiceUnavailable);
    }
    let source = &document.config;
    let resolved = runtime.snapshot().config();
    Ok(ConfigRead {
        state: document_state(store, &document)?,
        values: module_values(source, module),
        effective: effective_values(resolved, module),
        references: module_references(source, module),
        runtime: module_runtime(source, &runtime, coordinator, module)?,
    })
}

/// 系统读取只投影配置中的启动字段，不输出 users、hash、Secret 或解析后的绝对路径。
///
/// `restart_required` 只在这里暴露：已写入配置、但需重启进程才生效的启动级组件。
pub(crate) fn system_configuration(
    store: &ConfigStore,
    coordinator: &RuntimeCoordinator,
) -> Result<SystemConfigRead, ErrorCode> {
    let document = store.current();
    if document.runtime_revision != coordinator.load().revision().0 {
        return Err(ErrorCode::ServiceUnavailable);
    }
    let config = &document.config;
    Ok(SystemConfigRead {
        state: document_state(store, &document)?,
        work_path: display_path(&config.work.path),
        rules_path: display_path(&config.work.rules_path),
        database_path: display_path(&config.database.path),
        records_path: display_path(&config.database.records_path),
        webui_enabled: config.webui.enable,
        webui_address: config.webui.address.to_string(),
        webui_port: config.webui.port,
        public_origin: config.webui.public_origin.as_ref().map(ToString::to_string),
        restart_required: document
            .restart_required
            .iter()
            .filter_map(|component| RestartComponent::from_component(component))
            .collect(),
    })
}

fn module_values(config: &ConfigV2, module: ConfigModule) -> Vec<ModuleSource> {
    match module {
        ConfigModule::Listener => config
            .listener
            .iter()
            .cloned()
            .map(ModuleSource::Listener)
            .collect(),
        ConfigModule::Upstreams => config
            .upstreams
            .iter()
            .cloned()
            .map(ModuleSource::Upstreams)
            .collect(),
        ConfigModule::Strategy => config
            .strategy
            .iter()
            .cloned()
            .map(ModuleSource::Strategy)
            .collect(),
        ConfigModule::Hosts => config
            .hosts
            .iter()
            .cloned()
            .map(ModuleSource::Hosts)
            .collect(),
        ConfigModule::Outbound => config
            .outbound
            .iter()
            .cloned()
            .map(ModuleSource::Outbound)
            .collect(),
        ConfigModule::RuleSet => config
            .rule_set
            .iter()
            .cloned()
            .map(ModuleSource::RuleSet)
            .collect(),
        ConfigModule::Clients => config
            .clients
            .iter()
            .cloned()
            .map(ModuleSource::Clients)
            .collect(),
        ConfigModule::Dns => vec![ModuleSource::Dns(config.dns.clone())],
        ConfigModule::Statistics => vec![ModuleSource::Statistics(config.statistics.clone())],
        ConfigModule::Logs => vec![ModuleSource::Logs(config.logs.clone())],
    }
}

fn effective_values(config: &ResolvedConfig, module: ConfigModule) -> Vec<EffectiveField> {
    let mut fields = Vec::new();
    match module {
        ConfigModule::Dns => {
            fields.extend([
                field(
                    "dns.cache.enabled",
                    ValueOrigin::Global,
                    config.dns.cache.enabled,
                ),
                field(
                    "dns.cache.memory.max_size_bytes",
                    ValueOrigin::Global,
                    config.dns.cache.memory_max_size_bytes,
                ),
                field(
                    "dns.cache.failure_ttl",
                    ValueOrigin::Global,
                    duration_value(config.dns.cache.failure_ttl),
                ),
                field(
                    "dns.cache.negative_ttl_max",
                    ValueOrigin::Global,
                    duration_value(config.dns.cache.negative_ttl_max),
                ),
                field(
                    "dns.cache.optimistic.enabled",
                    ValueOrigin::Global,
                    config.dns.cache.optimistic.enabled,
                ),
                field(
                    "dns.cache.persistence.enabled",
                    ValueOrigin::Global,
                    config.dns.cache.persistence_enabled,
                ),
                field(
                    "dns.resolve_log.enable",
                    ValueOrigin::Global,
                    config.dns.resolve_log.enable,
                ),
            ]);
            push_ttl_fields(&mut fields, "dns.ttl_override", &config.dns.ttl_override);
            push_ecs_fields(
                &mut fields,
                "dns.edns_client_subnet",
                &config.dns.edns_client_subnet,
            );
        }
        ConfigModule::Statistics => fields.extend([
            field(
                "statistics.retention.days",
                ValueOrigin::Global,
                u64::from(config.statistics.retention_days),
            ),
            field(
                "statistics.retention.grace_days",
                ValueOrigin::Global,
                u64::from(config.statistics.retention_grace_days),
            ),
            field(
                "statistics.retention.reference_size_bytes",
                ValueOrigin::Global,
                config.statistics.retention_reference_size_bytes,
            ),
        ]),
        ConfigModule::Strategy => {
            for strategy in &config.strategies {
                let root = format!("strategy.{}", strategy.id.as_str());
                let (enabled, source) = strategy
                    .cache
                    .as_ref()
                    .map_or((config.dns.cache.enabled, ValueOrigin::Global), |cache| {
                        (cache.enabled, value_origin(cache.source))
                    });
                fields.push(field(format!("{root}.cache.enabled"), source, enabled));
                push_ttl_fields(
                    &mut fields,
                    &format!("{root}.ttl_override"),
                    &strategy.ttl_override,
                );
                push_ecs_fields(
                    &mut fields,
                    &format!("{root}.edns_client_subnet"),
                    &strategy.edns_client_subnet,
                );
            }
        }
        ConfigModule::Clients => {
            for client in &config.clients {
                let root = format!("clients.{}", client.name.as_str());
                let inherited_cache = client
                    .strategy
                    .as_ref()
                    .and_then(|id| config.strategies.iter().find(|item| &item.id == id))
                    .and_then(|strategy| strategy.cache.as_ref());
                let (enabled, source) = client
                    .cache
                    .as_ref()
                    .or(inherited_cache)
                    .map_or((config.dns.cache.enabled, ValueOrigin::Global), |cache| {
                        (cache.enabled, value_origin(cache.source))
                    });
                fields.push(field(format!("{root}.cache.enabled"), source, enabled));
                push_ttl_fields(
                    &mut fields,
                    &format!("{root}.ttl_override"),
                    &client.ttl_override,
                );
                push_ecs_fields(
                    &mut fields,
                    &format!("{root}.edns_client_subnet"),
                    &client.edns_client_subnet,
                );
            }
        }
        ConfigModule::Upstreams => {
            for upstream in &config.upstreams {
                if let ResolvedUpstream::Doh {
                    id,
                    edns_client_subnet: Some(ecs),
                    ..
                } = upstream
                {
                    push_ecs_fields(
                        &mut fields,
                        &format!("upstreams.{}.edns_client_subnet", id.as_str()),
                        ecs,
                    );
                }
            }
        }
        ConfigModule::Listener
        | ConfigModule::Hosts
        | ConfigModule::Outbound
        | ConfigModule::RuleSet
        | ConfigModule::Logs => {}
    }
    fields
}

fn module_references(config: &ConfigV2, module: ConfigModule) -> Vec<ResourceReference> {
    let mut references = Vec::new();
    match module {
        ConfigModule::Listener => {
            for listener in &config.listener {
                if let Some((_, _, strategy, hosts)) = listener.stream_details() {
                    push_reference(
                        &mut references,
                        module,
                        listener.name(),
                        "strategy",
                        strategy,
                    );
                    if let Some(hosts) = hosts {
                        push_reference(&mut references, module, listener.name(), "hosts", hosts);
                    }
                }
                if let Some((routes, _)) = listener.doh_details() {
                    for (index, route) in routes.iter().enumerate() {
                        push_reference(
                            &mut references,
                            module,
                            listener.name(),
                            &format!("routes[{index}].strategy"),
                            &route.strategy,
                        );
                    }
                }
            }
        }
        ConfigModule::Upstreams => {
            for upstream in &config.upstreams {
                if let Some((_, bootstrap, _, proxy, _)) = upstream.doh_details() {
                    if let Some(bootstrap) = bootstrap {
                        push_reference(
                            &mut references,
                            module,
                            upstream.name(),
                            "bootstrap",
                            bootstrap,
                        );
                    }
                    if let Some(proxy) = proxy {
                        push_reference(&mut references, module, upstream.name(), "proxy", proxy);
                    }
                }
                if let Some((members, _, _, fallbacks, _, _)) = upstream.group_details() {
                    for (index, member) in members.iter().enumerate() {
                        push_reference(
                            &mut references,
                            module,
                            upstream.name(),
                            &format!("upstreams[{index}].name"),
                            &member.name,
                        );
                    }
                    for (index, member) in fallbacks.unwrap_or_default().iter().enumerate() {
                        push_reference(
                            &mut references,
                            module,
                            upstream.name(),
                            &format!("fallbacks[{index}].name"),
                            &member.name,
                        );
                    }
                }
            }
        }
        ConfigModule::Strategy => {
            for strategy in &config.strategy {
                push_reference(
                    &mut references,
                    module,
                    &strategy.name,
                    "default_upstream",
                    &strategy.default_upstream,
                );
                for (index, rule) in strategy.rules.iter().enumerate() {
                    for (suffix, target) in [
                        ("rule_set", rule.rule_set.as_ref()),
                        ("hosts", rule.hosts.as_ref()),
                        ("upstream", rule.upstream.as_ref()),
                    ] {
                        if let Some(target) = target {
                            push_reference(
                                &mut references,
                                module,
                                &strategy.name,
                                &format!("rules[{index}].{suffix}"),
                                target,
                            );
                        }
                    }
                }
            }
        }
        ConfigModule::RuleSet => {
            for rule_set in &config.rule_set {
                if let Some((_, _, Some(proxy), _, _)) = rule_set.remote_details() {
                    push_reference(&mut references, module, rule_set.name(), "proxy", proxy);
                }
            }
        }
        ConfigModule::Clients => {
            for client in &config.clients {
                if let Some(strategy) = &client.strategy {
                    push_reference(&mut references, module, &client.name, "strategy", strategy);
                }
            }
        }
        ConfigModule::Hosts
        | ConfigModule::Outbound
        | ConfigModule::Dns
        | ConfigModule::Statistics
        | ConfigModule::Logs => {}
    }
    references
}

fn module_runtime(
    source: &ConfigV2,
    runtime: &crate::runtime::ActiveRuntime,
    coordinator: &RuntimeCoordinator,
    module: ConfigModule,
) -> Result<Vec<ModuleRuntime>, ErrorCode> {
    let mut values = Vec::new();
    match module {
        ConfigModule::Listener => {
            for listener in &source.listener {
                let bindings = runtime
                    .snapshot()
                    .config()
                    .bind_plan
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.owner == listener.name()
                            || entry
                                .doh_binding
                                .as_ref()
                                .is_some_and(|binding| binding.listener_id == listener.name())
                    })
                    .map(|entry| ListenerBinding {
                        endpoint_name: entry
                            .doh_binding
                            .as_ref()
                            .map(|binding| binding.endpoint_id.clone()),
                        address: entry.address.to_string(),
                        port: entry.port,
                        transport: match entry.transport {
                            crate::config::BindTransport::Udp => Transport::Udp,
                            crate::config::BindTransport::Tcp => Transport::Tcp,
                            crate::config::BindTransport::Doh => Transport::Doh,
                        },
                        accepting: !runtime.is_draining(),
                    })
                    .collect();
                values.push(ModuleRuntime::Listener {
                    name: listener.name().to_owned(),
                    bindings,
                });
            }
        }
        ConfigModule::Hosts => {
            for resource in &source.hosts {
                values.push(resource_runtime(
                    runtime,
                    resource.name(),
                    resource_update_interval(resource),
                    true,
                )?);
            }
        }
        ConfigModule::RuleSet => {
            for resource in &source.rule_set {
                values.push(resource_runtime(
                    runtime,
                    resource.name(),
                    rule_set_update_interval(resource),
                    false,
                )?);
            }
        }
        ConfigModule::Dns => values.push(ModuleRuntime::Dns {
            snapshot: cache_snapshot_status(runtime, coordinator, source)?,
        }),
        ConfigModule::Upstreams
        | ConfigModule::Strategy
        | ConfigModule::Outbound
        | ConfigModule::Clients
        | ConfigModule::Statistics
        | ConfigModule::Logs => {}
    }
    Ok(values)
}

fn resource_runtime(
    runtime: &crate::runtime::ActiveRuntime,
    name: &str,
    update_interval: Option<Duration>,
    hosts: bool,
) -> Result<ModuleRuntime, ErrorCode> {
    let id =
        crate::config::resolve::ConfigId::new(name).map_err(|_| ErrorCode::ServiceUnavailable)?;
    let snapshot = runtime.snapshot().resources().lookup(&id);
    let (condition, last_updated_at_ms, next_update_at_ms, error) = snapshot.map_or(
        (
            ResourceCondition::Unavailable,
            None,
            None,
            Some(ErrorCode::ServiceUnavailable),
        ),
        |snapshot| {
            let last = unix_millis(snapshot.fetched_at());
            let condition = match snapshot.stale_status() {
                ResourceStaleStatus::Fresh => ResourceCondition::Ready,
                ResourceStaleStatus::Stale => ResourceCondition::Stale,
            };
            let next = update_interval.and_then(|interval| {
                snapshot
                    .fetched_at()
                    .checked_add(interval)
                    .and_then(unix_millis)
            });
            (condition, last, next, None)
        },
    );
    Ok(if hosts {
        ModuleRuntime::Hosts {
            name: name.to_owned(),
            condition,
            last_updated_at_ms,
            next_update_at_ms,
            error,
        }
    } else {
        ModuleRuntime::RuleSet {
            name: name.to_owned(),
            condition,
            last_updated_at_ms,
            next_update_at_ms,
            error,
        }
    })
}

fn cache_snapshot_status(
    runtime: &crate::runtime::ActiveRuntime,
    coordinator: &RuntimeCoordinator,
    source: &ConfigV2,
) -> Result<CacheSnapshotStatus, ErrorCode> {
    let status = coordinator.cache_snapshot_status();
    let Some(status) = status else {
        return Ok(CacheSnapshotStatus {
            state: if source
                .dns
                .cache
                .as_ref()
                .is_some_and(|cache| cache.persistence.enabled)
            {
                SnapshotCondition::Failed
            } else {
                SnapshotCondition::Disabled
            },
            owner_revision: revision(runtime.revision().0.to_string())?,
            generation: DecimalU64::from(0),
            file_bytes: None,
            last_success_at_ms: None,
            last_error: source
                .dns
                .cache
                .as_ref()
                .is_some_and(|cache| cache.persistence.enabled)
                .then_some(ErrorCode::ServiceUnavailable),
        });
    };
    Ok(CacheSnapshotStatus {
        state: match status.condition {
            CacheSnapshotCondition::Disabled => SnapshotCondition::Disabled,
            CacheSnapshotCondition::Idle => SnapshotCondition::Idle,
            CacheSnapshotCondition::Writing => SnapshotCondition::Writing,
            CacheSnapshotCondition::Restoring => SnapshotCondition::Restoring,
            CacheSnapshotCondition::Failed | CacheSnapshotCondition::Stopped => {
                SnapshotCondition::Failed
            }
        },
        owner_revision: revision(status.owner_revision.0.to_string())?,
        generation: DecimalU64::from(status.generation),
        file_bytes: status.file_bytes.map(DecimalU64::from),
        last_success_at_ms: status.last_success_at_utc_millis,
        last_error: status.last_error.map(cache_snapshot_failure),
    })
}

fn cache_snapshot_failure(_failure: CacheSnapshotFailure) -> ErrorCode {
    ErrorCode::ServiceUnavailable
}

fn resource_update_interval(resource: &HostsResourceDto) -> Option<Duration> {
    resource
        .file_details()
        .and_then(|(_, _, auto_update, interval)| auto_update.then_some(interval).flatten())
}

fn rule_set_update_interval(resource: &RuleSetDto) -> Option<Duration> {
    resource
        .file_details()
        .and_then(|(_, _, auto_update, interval)| auto_update.then_some(interval).flatten())
        .or_else(|| {
            resource
                .remote_details()
                .and_then(|(_, _, _, auto_update, interval)| {
                    auto_update.then_some(interval).flatten()
                })
        })
}

fn push_reference(
    output: &mut Vec<ResourceReference>,
    from_module: ConfigModule,
    from_name: &str,
    path: &str,
    to_name: &str,
) {
    output.push(ResourceReference {
        from_module,
        from_name: from_name.to_owned(),
        path: path.to_owned(),
        to_name: to_name.to_owned(),
    });
}

fn push_ttl_fields(
    output: &mut Vec<EffectiveField>,
    root: &str,
    ttl: &crate::config::resolve::ResolvedTtlOverride,
) {
    let source = value_origin(ttl.source);
    output.push(field(
        format!("{root}.enabled"),
        source.clone(),
        ttl.enabled,
    ));
    if let Some(value) = ttl.min {
        output.push(field(
            format!("{root}.min"),
            source.clone(),
            duration_value(value),
        ));
    }
    if let Some(value) = ttl.max {
        output.push(field(format!("{root}.max"), source, duration_value(value)));
    }
}

fn push_ecs_fields(
    output: &mut Vec<EffectiveField>,
    root: &str,
    ecs: &crate::config::resolve::ResolvedEcs,
) {
    let source = value_origin(ecs.source);
    output.push(field(
        format!("{root}.mode"),
        source.clone(),
        format!("{:?}", ecs.mode).to_ascii_lowercase(),
    ));
    if let Some(value) = ecs.custom_ip {
        output.push(field(
            format!("{root}.custom_ip"),
            source,
            value.to_string(),
        ));
    }
}

fn value_origin(source: ValueSource) -> ValueOrigin {
    match source {
        ValueSource::Default => ValueOrigin::Default,
        ValueSource::Global => ValueOrigin::Global,
        ValueSource::Strategy => ValueOrigin::Strategy,
        ValueSource::Client => ValueOrigin::Client,
        ValueSource::Upstream | ValueSource::Rule => ValueOrigin::Explicit,
    }
}

fn field(
    path: impl Into<String>,
    source: ValueOrigin,
    value: impl Into<ScalarValue>,
) -> EffectiveField {
    EffectiveField {
        path: path.into(),
        source,
        value: value.into(),
    }
}

impl From<String> for ScalarValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<bool> for ScalarValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<u64> for ScalarValue {
    fn from(value: u64) -> Self {
        Self::Integer(value)
    }
}

/// 生效值回显与源 DTO 共用 `format_duration`，避免同一个 duration 在两个出口写成不同形态。
fn duration_value(value: Duration) -> String {
    format_duration(value)
}

fn display_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn unix_millis(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn revision(value: String) -> Result<Revision, ErrorCode> {
    Revision::try_from(value).map_err(|_| ErrorCode::ServiceUnavailable)
}

#[cfg(test)]
mod tests;
