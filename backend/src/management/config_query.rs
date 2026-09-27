//! v2 配置状态的白名单投影；活动正文来自 ConfigStore，effective/runtime 来自同代 Runtime。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::contract::{
    CacheSnapshotStatus, ConfigModule, ConfigRead, ConfigState, DecimalU64, EffectiveField,
    ErrorCode, FileCondition, FileObservation, ListenerBinding, ModuleRuntime, ModuleSource,
    OperationId, OperationResult, OperationStatus, ResourceCondition, ResourceReference, Revision,
    ScalarValue, SnapshotCondition, SyncCondition, SystemConfigRead, Transport, ValueOrigin,
};
use crate::cache::{CacheSnapshotCondition, CacheSnapshotFailure};
use crate::config::contract::ConfigV2;
use crate::config::model::{HostsResourceDto, RuleSetDto};
use crate::config::resolve::{ResolvedConfig, ResolvedUpstream, ValueSource};
use crate::config::store::{
    ConfigStore,
    active::{ActiveError, ConfigurationStatus, OperationFailure, OperationSnapshot},
    observation::FileObservation as ObservedFile,
};
use crate::resource::ResourceStaleStatus;
use crate::runtime::RuntimeCoordinator;

pub(crate) mod external;

/// 状态查询不做文件 I/O；外部差异与持久化结果独立展示，不把文件变化解释成自动应用。
pub(crate) fn configuration_state(store: &ConfigStore) -> Result<ConfigState, ErrorCode> {
    let status = store.configuration_status().map_err(error_code)?;
    configuration_state_from_status(&status)
}

/// 从同一次锁内冻结结果投影状态，供模块与系统读取避免二次读取跨代。
fn configuration_state_from_status(status: &ConfigurationStatus) -> Result<ConfigState, ErrorCode> {
    let active = &status.active;
    let synchronization = match &status.operation {
        Some(OperationSnapshot::Preparing | OperationSnapshot::Applying) => SyncCondition::Applying,
        Some(OperationSnapshot::Persisting { .. }) => SyncCondition::Persisting,
        Some(OperationSnapshot::AppliedUnpersisted { .. }) => SyncCondition::AppliedUnpersisted,
        Some(_) => SyncCondition::Blocked,
        None if active.operation_id.is_none()
            && active.persisted_revision.as_ref() == Some(&active.revision) =>
        {
            SyncCondition::Synced
        }
        None => SyncCondition::Blocked,
    };
    Ok(ConfigState {
        active_revision: revision(active.revision.clone())?,
        // RuntimeRevision 的 u64 只编码为十进制 opaque revision，不输出不安全 JSON number。
        runtime_revision: revision(active.runtime_revision.to_string())?,
        persisted_revision: active
            .persisted_revision
            .clone()
            .map(revision)
            .transpose()?,
        observed_file_revision: revision(active.observation.revision())?,
        files: FileObservation {
            source: file_condition(&active.observation.source, Some(&status.known_files.source)),
            derived: active
                .observation
                .derived
                .as_ref()
                .map(|file| file_condition(file, status.known_files.derived.as_ref())),
        },
        synchronization,
        operation_id: active
            .operation_id
            .as_ref()
            .map(|id| OperationId::try_from(id.clone()).map_err(|_| ErrorCode::ServiceUnavailable))
            .transpose()?,
    })
}

/// 只返回请求模块的源配置、有效值、出站引用及运行态。
pub(crate) fn configuration_module(
    store: &ConfigStore,
    coordinator: &RuntimeCoordinator,
    module: ConfigModule,
) -> Result<ConfigRead, ErrorCode> {
    let status = store.configuration_status().map_err(error_code)?;
    let runtime = coordinator.load();
    if status.active.runtime_revision != runtime.revision().0 {
        return Err(ErrorCode::ServiceUnavailable);
    }
    let source = &status.active.config;
    let resolved = runtime.snapshot().config();
    Ok(ConfigRead {
        state: configuration_state_from_status(&status)?,
        values: module_values(source, module),
        effective: effective_values(resolved, module),
        references: module_references(source, module),
        runtime: module_runtime(source, &runtime, coordinator, module)?,
    })
}

/// 系统读取只投影活动源中的启动字段，不输出 users、hash、Secret 或解析后的绝对路径。
pub(crate) fn system_configuration(
    store: &ConfigStore,
    coordinator: &RuntimeCoordinator,
) -> Result<SystemConfigRead, ErrorCode> {
    let status = store.configuration_status().map_err(error_code)?;
    if status.active.runtime_revision != coordinator.load().revision().0 {
        return Err(ErrorCode::ServiceUnavailable);
    }
    let config = &status.active.config;
    Ok(SystemConfigRead {
        state: configuration_state_from_status(&status)?,
        work_path: display_path(&config.work.path),
        rules_path: display_path(&config.work.rules_path),
        database_path: display_path(&config.database.path),
        records_path: display_path(&config.database.records_path),
        webui_enabled: config.webui.enable,
        webui_address: config.webui.address.to_string(),
        webui_port: config.webui.port,
        public_origin: config.webui.public_origin.as_ref().map(ToString::to_string),
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

fn duration_value(value: Duration) -> String {
    format!("{}ns", value.as_nanos())
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

/// adapter 仍须先鉴权；此读口按原调用者返回冻结结果，Unknown 不能触发自动重放。
pub(crate) fn operation_result(
    store: &ConfigStore,
    actor: &str,
    operation_id: &str,
) -> Result<OperationResult, ErrorCode> {
    let id =
        OperationId::try_from(operation_id.to_owned()).map_err(|_| ErrorCode::InvalidArgument)?;
    let result = store
        .operation_snapshot(actor, operation_id)
        .map_err(error_code)?;
    let status = match result {
        OperationSnapshot::Preparing => OperationStatus::Preparing {},
        OperationSnapshot::Applying => OperationStatus::Applying {},
        OperationSnapshot::Persisting { active_revision } => OperationStatus::Persisting {
            active_revision: revision(active_revision)?,
        },
        OperationSnapshot::AppliedSynced {
            active_revision,
            persisted_revision,
        } => OperationStatus::AppliedSynced {
            active_revision: revision(active_revision)?,
            persisted_revision: revision(persisted_revision)?,
        },
        OperationSnapshot::AppliedUnpersisted {
            active_revision,
            persisted_revision,
            error,
        } => OperationStatus::AppliedUnpersisted {
            active_revision: revision(active_revision)?,
            persisted_revision: persisted_revision.map(revision).transpose()?,
            error: failure_code(error),
        },
        OperationSnapshot::Rejected { error } => OperationStatus::Rejected {
            error: failure_code(error),
        },
        OperationSnapshot::CompensationFailed {
            active_revision,
            error,
        } => OperationStatus::CompensationFailed {
            active_revision: active_revision.map(revision).transpose()?,
            error: failure_code(error),
        },
        OperationSnapshot::Unknown => OperationStatus::Unknown {},
    };
    Ok(OperationResult {
        operation_id: id,
        status,
    })
}

fn revision(value: String) -> Result<Revision, ErrorCode> {
    Revision::try_from(value).map_err(|_| ErrorCode::ServiceUnavailable)
}

fn file_condition(current: &ObservedFile, known: Option<&ObservedFile>) -> FileCondition {
    match current {
        ObservedFile::Missing => FileCondition::Missing,
        ObservedFile::Unreadable => FileCondition::Unreadable,
        ObservedFile::Oversized => FileCondition::Oversized,
        ObservedFile::Readable { .. } if Some(current) == known => FileCondition::Unchanged,
        ObservedFile::Readable { .. } => FileCondition::Changed,
    }
}

fn failure_code(error: OperationFailure) -> ErrorCode {
    match error {
        OperationFailure::ValidationFailed => ErrorCode::ValidationFailed,
        OperationFailure::ActiveRevisionConflict => ErrorCode::ActiveRevisionConflict,
        OperationFailure::FileRevisionConflict => ErrorCode::FileRevisionConflict,
        OperationFailure::ApplyFailed => ErrorCode::ApplyFailed,
        OperationFailure::PersistenceFailed => ErrorCode::PersistenceFailed,
        OperationFailure::CompensationFailed => ErrorCode::CompensationFailed,
    }
}

pub(super) fn error_code(error: ActiveError) -> ErrorCode {
    match error {
        ActiveError::Busy => ErrorCode::OperationBusy,
        ActiveError::InvalidToken => ErrorCode::InvalidArgument,
        ActiveError::ActiveConflict => ErrorCode::ActiveRevisionConflict,
        ActiveError::FileConflict => ErrorCode::FileRevisionConflict,
        ActiveError::Unavailable | ActiveError::Entropy => ErrorCode::ServiceUnavailable,
        ActiveError::ExternalConfirmation => ErrorCode::ExternalChangesRequireConfirmation,
        ActiveError::ValidationExpired => ErrorCode::ValidationExpired,
        ActiveError::OperationIdReused => ErrorCode::OperationIdReused,
        ActiveError::MissingConfirmation | ActiveError::Candidate(_) => ErrorCode::ValidationFailed,
        ActiveError::Persistence(_) => ErrorCode::PersistenceFailed,
    }
}

#[cfg(test)]
mod tests;
