//! 活动源上的受限变更及类型化引用维护；不读取外部文件，也不序列化 ResolvedConfig。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use yaml_serde::Value;

use super::contract::{ClientMatchV2, ClientV2, ConfigV2, DnsV2, StatisticsV2};
use super::model::{
    CacheOverrideDto, EcsDto, HostsResourceDto, ListenerDto, LogsDto, OutboundDto, RuleSetDto,
    StrategyDto, TtlOverrideDto, UpstreamDto,
};
use super::source_edit::edit_document;
use super::validate::ConfigErrorReport;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigModule {
    Listener,
    Upstreams,
    Strategy,
    Hosts,
    Outbound,
    RuleSet,
    Clients,
    Dns,
    Statistics,
    Logs,
}

impl ConfigModule {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Listener => "listener",
            Self::Upstreams => "upstreams",
            Self::Strategy => "strategy",
            Self::Hosts => "hosts",
            Self::Outbound => "outbound",
            Self::RuleSet => "rule_set",
            Self::Clients => "clients",
            Self::Dns => "dns",
            Self::Statistics => "statistics",
            Self::Logs => "logs",
        }
    }
}

/// 创建不携带旧名；更新始终由活动快照中的旧 name 定位，没有顶层删除或通用 Patch。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceMutation<T> {
    Create { value: T },
    Update { original_name: String, value: T },
}

/// 普通编辑只允许此白名单；client_id 不随改名或差异采用改变。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientEdit {
    pub name: String,
    #[serde(default)]
    pub r#match: ClientMatchV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheOverrideDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_override: Option<TtlOverrideDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edns_client_subnet: Option<EcsDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMutation {
    Create {
        value: ClientV2,
    },
    Update {
        original_name: String,
        value: ClientEdit,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "module",
    content = "change",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ConfigChange {
    Listener(ResourceMutation<ListenerDto>),
    Upstreams(ResourceMutation<UpstreamDto>),
    Strategy(ResourceMutation<StrategyDto>),
    Hosts(ResourceMutation<HostsResourceDto>),
    Outbound(ResourceMutation<OutboundDto>),
    RuleSet(ResourceMutation<RuleSetDto>),
    Clients(ClientMutation),
    Dns(DnsV2),
    Statistics(StatisticsV2),
    Logs(LogsDto),
}

impl ConfigChange {
    pub fn module(&self) -> ConfigModule {
        match self {
            Self::Listener(_) => ConfigModule::Listener,
            Self::Upstreams(_) => ConfigModule::Upstreams,
            Self::Strategy(_) => ConfigModule::Strategy,
            Self::Hosts(_) => ConfigModule::Hosts,
            Self::Outbound(_) => ConfigModule::Outbound,
            Self::RuleSet(_) => ConfigModule::RuleSet,
            Self::Clients(_) => ConfigModule::Clients,
            Self::Dns(_) => ConfigModule::Dns,
            Self::Statistics(_) => ConfigModule::Statistics,
            Self::Logs(_) => ConfigModule::Logs,
        }
    }
}

/// 只有严格解析、完整引用校验、路径校验和编辑后等价核对均通过才能构造。
/// 此类型不代表资源、socket 或进程 owner 已经 prepare。
pub(crate) struct SourceCandidate {
    pub(crate) source: String,
    pub(crate) config: ConfigV2,
    pub(crate) renamed: bool,
}

#[derive(Debug, Error)]
pub(crate) enum EditError {
    #[error("candidate must contain 1..=128 changes within 2 MiB")]
    Budget,
    #[error("candidate addresses the same resource more than once")]
    DuplicateTarget,
    #[error("original resource does not exist in the active configuration")]
    NotFound,
    #[error("candidate cannot be represented by the supported source editor")]
    UnsupportedSource,
    #[error("candidate validation failed: {0}")]
    Validation(ConfigErrorReport),
}

impl From<ConfigErrorReport> for EditError {
    fn from(report: ConfigErrorReport) -> Self {
        Self::Validation(report)
    }
}

/// 所有旧键均相对于同一活动源定位，最后一次性校验整张引用图，允许合法的组合变更。
pub(crate) fn build_candidate(
    source: &str,
    source_path: &Path,
    changes: &[ConfigChange],
) -> Result<SourceCandidate, EditError> {
    if changes.is_empty()
        || changes.len() > 128
        || serde_json::to_vec(changes)
            .map_err(|_| EditError::UnsupportedSource)?
            .len()
            > 2 * 1024 * 1024
    {
        return Err(EditError::Budget);
    }
    let original = ConfigV2::parse(source.as_bytes())?;
    let original_tree: Value =
        yaml_serde::from_str(source).map_err(|_| EditError::UnsupportedSource)?;
    let mut tree = original_tree.clone();
    let mut touched = BTreeSet::new();
    let mut renames = BTreeMap::new();
    for change in changes {
        let module = change.module();
        let (old_name, value, canonical_old) = match change {
            ConfigChange::Listener(change) => {
                resource(change, &original.listener, ListenerDto::name)?
            }
            ConfigChange::Upstreams(change) => {
                resource(change, &original.upstreams, UpstreamDto::name)?
            }
            ConfigChange::Strategy(change) => {
                resource(change, &original.strategy, |item| &item.name)?
            }
            ConfigChange::Hosts(change) => {
                resource(change, &original.hosts, HostsResourceDto::name)?
            }
            ConfigChange::Outbound(change) => {
                resource(change, &original.outbound, |item| &item.name)?
            }
            ConfigChange::RuleSet(change) => {
                resource(change, &original.rule_set, RuleSetDto::name)?
            }
            ConfigChange::Clients(ClientMutation::Create { value }) => {
                (None, to_value(value)?, None)
            }
            ConfigChange::Clients(ClientMutation::Update {
                original_name,
                value,
            }) => {
                let old = original
                    .clients
                    .iter()
                    .find(|item| item.name == *original_name)
                    .ok_or(EditError::NotFound)?;
                let mut next = to_value(value)?;
                next["client_id"] = Value::String(old.client_id.clone());
                (Some(original_name.as_str()), next, Some(to_value(old)?))
            }
            ConfigChange::Dns(value) => (None, to_value(value)?, Some(to_value(&original.dns)?)),
            ConfigChange::Statistics(value) => (
                None,
                to_value(value)?,
                Some(to_value(&original.statistics)?),
            ),
            ConfigChange::Logs(value) => (None, to_value(value)?, Some(to_value(&original.logs)?)),
        };
        let single = matches!(
            module,
            ConfigModule::Dns | ConfigModule::Statistics | ConfigModule::Logs
        );
        let new_name = if single {
            ""
        } else {
            value["name"].as_str().ok_or(EditError::UnsupportedSource)?
        };
        let key = (module, old_name.unwrap_or(new_name).to_owned());
        if !touched.insert(key) {
            return Err(EditError::DuplicateTarget);
        }
        if single {
            let slot = tree
                .as_mapping_mut()
                .ok_or(EditError::UnsupportedSource)?
                .entry(Value::String(module.key().into()))
                .or_insert_with(|| Value::Mapping(Default::default()));
            merge_changed(slot, canonical_old.as_ref().unwrap(), &value);
        } else if let Some(old_name) = old_name {
            // 索引来自旧树，不按已经改名的中间树查找，因此交换名称不会错误命中。
            let index = original_tree[module.key()]
                .as_sequence()
                .and_then(|items| {
                    items
                        .iter()
                        .position(|item| item["name"].as_str() == Some(old_name))
                })
                .ok_or(EditError::NotFound)?;
            merge_changed(
                &mut tree[module.key()][index],
                canonical_old.as_ref().unwrap(),
                &value,
            );
            if old_name != new_name {
                renames.insert((module, old_name.to_owned()), new_name.to_owned());
            }
        } else {
            let items = tree
                .as_mapping_mut()
                .ok_or(EditError::UnsupportedSource)?
                .entry(Value::String(module.key().into()))
                .or_insert_with(|| Value::Sequence(Vec::new()));
            items
                .as_sequence_mut()
                .ok_or(EditError::UnsupportedSource)?
                .push(value);
        }
    }
    // 引用只做一次旧键 -> 新键映射，不递归替换、不触碰内联正文、路径或 SecretRef。
    rewrite_references(&mut tree, &renames)?;
    let intended = yaml_serde::to_string(&tree).map_err(|_| EditError::UnsupportedSource)?;
    let config = ConfigV2::parse(intended.as_bytes())?;
    config.resolve_paths(source_path)?;
    let edited =
        edit_document(source, &original_tree, &tree).map_err(|_| EditError::UnsupportedSource)?;
    let reparsed = ConfigV2::parse(edited.as_bytes())?;
    reparsed.resolve_paths(source_path)?;
    Ok(SourceCandidate {
        source: edited,
        config,
        renamed: !renames.is_empty(),
    })
}

fn to_value(value: &impl Serialize) -> Result<Value, EditError> {
    yaml_serde::to_value(value).map_err(|_| EditError::UnsupportedSource)
}

fn resource<'a, T: Serialize>(
    change: &'a ResourceMutation<T>,
    original: &[T],
    name: impl Fn(&T) -> &str,
) -> Result<(Option<&'a str>, Value, Option<Value>), EditError> {
    match change {
        ResourceMutation::Create { value } => Ok((None, to_value(value)?, None)),
        ResourceMutation::Update {
            original_name,
            value,
        } => {
            let old = original
                .iter()
                .find(|item| name(item) == original_name)
                .ok_or(EditError::NotFound)?;
            Ok((Some(original_name), to_value(value)?, Some(to_value(old)?)))
        }
    }
}

/// 只写 DTO 中确实改变的字段，保留原文中未变化的缺省、相对路径和 duration/IP 写法。
fn merge_changed(source: &mut Value, old: &Value, new: &Value) {
    if old == new {
        return;
    }
    match (source, old, new) {
        (Value::Mapping(source), Value::Mapping(old), Value::Mapping(new)) => {
            for key in old.keys().filter(|key| !new.contains_key(*key)) {
                source.remove(key);
            }
            for (key, value) in new {
                if let Some(previous) = old.get(key) {
                    if previous != value {
                        merge_changed(
                            source
                                .entry(key.clone())
                                .or_insert_with(|| previous.clone()),
                            previous,
                            value,
                        );
                    }
                } else {
                    source.insert(key.clone(), value.clone());
                }
            }
        }
        (Value::Sequence(source), Value::Sequence(old), Value::Sequence(new))
            if source.len() == old.len() && old.len() == new.len() =>
        {
            for ((source, old), new) in source.iter_mut().zip(old).zip(new) {
                merge_changed(source, old, new);
            }
        }
        (source, _, new) => *source = new.clone(),
    }
}

fn rename(
    value: &mut Value,
    module: ConfigModule,
    renames: &BTreeMap<(ConfigModule, String), String>,
) {
    if let Some(name) = value.as_str()
        && let Some(next) = renames.get(&(module, name.to_owned()))
    {
        *value = Value::String(next.clone());
    }
}

fn rename_field(
    value: &mut Value,
    field: &str,
    module: ConfigModule,
    renames: &BTreeMap<(ConfigModule, String), String>,
) {
    if let Some(field) = value.as_mapping_mut().and_then(|item| item.get_mut(field)) {
        rename(field, module, renames);
    }
}

fn items(tree: &mut Value, module: ConfigModule) -> impl Iterator<Item = &mut Value> {
    tree.as_mapping_mut()
        .and_then(|tree| tree.get_mut(module.key()))
        .and_then(Value::as_sequence_mut)
        .into_iter()
        .flatten()
}

fn rewrite_references(
    tree: &mut Value,
    renames: &BTreeMap<(ConfigModule, String), String>,
) -> Result<(), EditError> {
    // 用实际 variant 决定字段的引用类型；未知 variant 留给严格 parser 拒绝。
    for listener in items(tree, ConfigModule::Listener) {
        match listener["type"].as_str() {
            Some("udp" | "tcp") => {
                rename_field(listener, "strategy", ConfigModule::Strategy, renames);
                rename_field(listener, "hosts", ConfigModule::Hosts, renames);
            }
            Some("doh") => {
                for route in listener["routes"]
                    .as_sequence_mut()
                    .ok_or(EditError::UnsupportedSource)?
                {
                    rename_field(route, "strategy", ConfigModule::Strategy, renames);
                }
            }
            _ => return Err(EditError::UnsupportedSource),
        }
    }
    for upstream in items(tree, ConfigModule::Upstreams) {
        match upstream["type"].as_str() {
            Some("doh") => {
                rename_field(upstream, "bootstrap", ConfigModule::Upstreams, renames);
                rename_field(upstream, "proxy", ConfigModule::Outbound, renames);
            }
            Some("group") => {
                for field in ["upstreams", "fallbacks"] {
                    if let Some(members) = upstream
                        .as_mapping_mut()
                        .and_then(|item| item.get_mut(field))
                        .and_then(Value::as_sequence_mut)
                    {
                        for member in members {
                            rename_field(member, "name", ConfigModule::Upstreams, renames);
                        }
                    }
                }
            }
            Some("hosts") => {}
            _ => return Err(EditError::UnsupportedSource),
        }
    }
    for strategy in items(tree, ConfigModule::Strategy) {
        rename_field(
            strategy,
            "default_upstream",
            ConfigModule::Upstreams,
            renames,
        );
        for rule in strategy["rules"]
            .as_sequence_mut()
            .ok_or(EditError::UnsupportedSource)?
        {
            rename_field(rule, "hosts", ConfigModule::Hosts, renames);
            rename_field(rule, "upstream", ConfigModule::Upstreams, renames);
            if let Some(reference) = rule["rule_set"].as_str() {
                // dat selector 使用 name:selector；name 字符集不含冒号。
                let (name, suffix) = reference
                    .split_once(':')
                    .map_or((reference, None), |(name, suffix)| (name, Some(suffix)));
                if let Some(next) = renames.get(&(ConfigModule::RuleSet, name.to_owned())) {
                    rule["rule_set"] = Value::String(
                        suffix.map_or_else(|| next.clone(), |suffix| format!("{next}:{suffix}")),
                    );
                }
            }
        }
    }
    for rule_set in items(tree, ConfigModule::RuleSet) {
        if rule_set["type"].as_str() == Some("remote") {
            rename_field(rule_set, "proxy", ConfigModule::Outbound, renames);
        }
    }
    for client in items(tree, ConfigModule::Clients) {
        rename_field(client, "strategy", ConfigModule::Strategy, renames);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
