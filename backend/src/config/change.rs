//! 配置变更模型、类型化合并（`apply_changes`）、引用改名及脱敏密码恢复。
//!
//! 本模块只在类型化配置上得到完整的目标配置，不负责保留原文写法；原文保留交给
//! [`super::writer`] 的条目级写回。不读取外部文件，也不序列化 ResolvedConfig。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use yaml_serde::Value;

use super::contract::{ClientMatchV2, ClientV2, ConfigV2, DnsV2, StatisticsV2};
use super::model::{
    CacheOverrideDto, EcsDto, HostsResourceDto, ListenerDto, LogsDto, OutboundDto,
    REDACTED_INLINE_PASSWORD, RuleSetDto, SecretRefDto, StrategyDto, TtlOverrideDto, UpstreamDto,
    to_source_value,
};
use super::validate::{ConfigError, ConfigErrorKind, ConfigErrorReport};
use super::writer::{self, WriteError, is_list_module, source_model};

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

/// 普通编辑白名单。`client_id` 可选：缺省沿用活动值，显式提供时替换请求身份；
/// 格式与唯一性由整份候选的契约校验统一拒绝。历史记录保留写入时的旧 ID，不迁移。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientEdit {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
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

/// 只有严格解析、完整引用校验、路径校验和写回语义复核均通过才能构造。
/// 此类型不代表资源、socket 或进程 owner 已经 prepare。
pub(crate) struct SourceCandidate {
    pub(crate) source: String,
    pub(crate) config: ConfigV2,
}

/// 本次请求中的改名：`(模块, 旧名) -> 新名`。引用只按这张表做一次映射，不链式改名。
pub(crate) type Renames = BTreeMap<(ConfigModule, String), String>;

/// `apply_changes` 的结果：已通过完整语义校验的目标配置及本次改名表。
pub(crate) struct AppliedChanges {
    pub(crate) config: ConfigV2,
    pub(crate) renames: Renames,
}

#[derive(Debug, Error)]
pub(crate) enum EditError {
    #[error("candidate must contain 1..=128 changes within 2 MiB")]
    Budget,
    #[error("candidate addresses the same resource more than once")]
    DuplicateTarget,
    #[error("original resource does not exist in the active configuration")]
    NotFound,
    /// 写回层无法安全修改源文件中该条目（含锚点/别名，或写回结果未通过语义复核）。
    #[error("configuration source layout of {module} cannot be edited safely")]
    Layout {
        module: &'static str,
        name: Option<String>,
    },
    /// 类型化值无法在 DTO 与 YAML 值之间往返；正常输入不会出现。
    #[error("candidate cannot be converted between typed and source representation")]
    Internal,
    #[error("candidate validation failed: {0}")]
    Validation(ConfigErrorReport),
}

impl From<ConfigErrorReport> for EditError {
    fn from(report: ConfigErrorReport) -> Self {
        Self::Validation(report)
    }
}

impl From<WriteError> for EditError {
    fn from(error: WriteError) -> Self {
        match error {
            WriteError::Layout { module, name } => Self::Layout { module, name },
            WriteError::Serialize => Self::Internal,
        }
    }
}

/// 内联地址在候选里的真实值只来自脱敏回填或用户新值；这里用活动源恢复未修改的密码，
/// 使原本只需改名等无关字段的编辑不再要求重新输入完整 URL。摘要与候选共用同一展开结果。
pub(crate) fn expand_redacted_changes(
    source: &str,
    changes: &[ConfigChange],
) -> Result<Vec<ConfigChange>, EditError> {
    let original = ConfigV2::parse(source.as_bytes())?;
    let mut expanded = changes.to_vec();
    for change in &mut expanded {
        // 改名编辑的新 name 在活动源中并不存在，必须按 original_name 定位原条目，
        // 否则脱敏占位符会被当作真实密码写入源文件。
        let ConfigChange::Outbound(ResourceMutation::Update {
            original_name,
            value,
        }) = change
        else {
            continue;
        };
        // 原条目缺失由 build_candidate 报 NotFound，这里保持展开步骤的单一职责。
        let Some(index) = original
            .outbound
            .iter()
            .position(|item| item.name == *original_name)
        else {
            continue;
        };
        value
            .proxy_url
            .restore_redacted_url(&original.outbound[index].proxy_url);
        // 占位符只有在能按活动源恢复时才有意义。用户改了 scheme/host/port、或原条目不是同址内联
        // 来源时无法恢复，此时必须让用户重填完整 URL，不能把占位符当作密码写进配置文件。
        if let SecretRefDto::Inline { url } = &value.proxy_url
            && url.contains(REDACTED_INLINE_PASSWORD)
        {
            let mut report = ConfigErrorReport::default();
            report.push(ConfigError::new(
                ConfigErrorKind::Secret,
                format!("outbound[{index}].proxy_url"),
                "redacted inline proxy URL cannot be restored; submit the full URL again",
            ));
            return Err(EditError::Validation(report));
        }
    }
    Ok(expanded)
}

/// 保存的候选构造入口：类型化合并后用条目级写回生成源文本，再严格解析并校验路径。
pub(crate) fn build_candidate(
    source: &str,
    source_path: &Path,
    changes: &[ConfigChange],
) -> Result<SourceCandidate, EditError> {
    let base = ConfigV2::parse(source.as_bytes())?;
    let applied = apply_changes(&base, changes)?;
    applied.config.resolve_paths(source_path)?;
    let text = writer::render(source, &base, &applied.config, &applied.renames)?;
    let config = ConfigV2::parse(text.as_bytes())?;
    config.resolve_paths(source_path)?;
    Ok(SourceCandidate {
        source: text,
        config,
    })
}

/// 在类型化配置上替换或追加条目、改写引用，得到完整且已通过语义校验的目标配置。
///
/// 所有旧名均相对于 `base` 定位，最后一次性校验整张引用图，允许合法的组合变更（例如同一请求
/// 内新增上游并让策略引用它）。值的比较与合并只使用源序列化，内联凭据不会被脱敏占位符覆盖。
pub(crate) fn apply_changes(
    base: &ConfigV2,
    changes: &[ConfigChange],
) -> Result<AppliedChanges, EditError> {
    if changes.is_empty()
        || changes.len() > 128
        || serde_json::to_vec(changes)
            .map_err(|_| EditError::Internal)?
            .len()
            > 2 * 1024 * 1024
    {
        return Err(EditError::Budget);
    }
    let model = source_model(base)?;
    let mut touched = BTreeSet::new();
    let mut renames = Renames::new();
    let mut tree: BTreeMap<ConfigModule, Value> = model.clone();
    for change in changes {
        let module = change.module();
        let (old_name, value) = match change {
            ConfigChange::Listener(change) => resource(change, &base.listener, ListenerDto::name)?,
            ConfigChange::Upstreams(change) => {
                resource(change, &base.upstreams, UpstreamDto::name)?
            }
            ConfigChange::Strategy(change) => resource(change, &base.strategy, |item| &item.name)?,
            ConfigChange::Hosts(change) => resource(change, &base.hosts, HostsResourceDto::name)?,
            ConfigChange::Outbound(change) => resource(change, &base.outbound, |item| &item.name)?,
            ConfigChange::RuleSet(change) => resource(change, &base.rule_set, RuleSetDto::name)?,
            ConfigChange::Clients(ClientMutation::Create { value }) => (None, to_value(value)?),
            ConfigChange::Clients(ClientMutation::Update {
                original_name,
                value,
            }) => {
                let old = base
                    .clients
                    .iter()
                    .find(|item| item.name == *original_name)
                    .ok_or(EditError::NotFound)?;
                let mut next = to_value(value)?;
                let client_id = value.client_id.as_ref().unwrap_or(&old.client_id);
                next["client_id"] = Value::String(client_id.clone());
                (Some(original_name.as_str()), next)
            }
            ConfigChange::Dns(value) => (None, to_value(value)?),
            ConfigChange::Statistics(value) => (None, to_value(value)?),
            ConfigChange::Logs(value) => (None, to_value(value)?),
        };
        let list = is_list_module(module);
        let new_name = if list {
            value["name"]
                .as_str()
                .ok_or(EditError::Internal)?
                .to_owned()
        } else {
            String::new()
        };
        let key = (module, old_name.unwrap_or(&new_name).to_owned());
        if !touched.insert(key) {
            return Err(EditError::DuplicateTarget);
        }
        let slot = tree.get_mut(&module).ok_or(EditError::Internal)?;
        if !list {
            *slot = value;
        } else if let Some(old_name) = old_name {
            // 索引来自 base，不按已经改名的中间结果查找，因此互换名称不会错误命中。
            let index = model[&module]
                .as_sequence()
                .and_then(|items| {
                    items
                        .iter()
                        .position(|item| item["name"].as_str() == Some(old_name))
                })
                .ok_or(EditError::NotFound)?;
            let items = slot.as_sequence_mut().ok_or(EditError::Internal)?;
            items[index] = value;
            if old_name != new_name {
                renames.insert((module, old_name.to_owned()), new_name);
            }
        } else {
            slot.as_sequence_mut()
                .ok_or(EditError::Internal)?
                .push(value);
        }
    }
    // 引用只做一次旧键 -> 新键映射，不递归替换、不触碰内联正文、路径或 SecretRef。
    let mut tree: Value = Value::Mapping(
        tree.into_iter()
            .map(|(module, value)| (Value::String(module.key().into()), value))
            .collect(),
    );
    rewrite_references(&mut tree, &renames)?;
    let mut config = base.clone();
    config.listener = from_tree(&tree, ConfigModule::Listener)?;
    config.upstreams = from_tree(&tree, ConfigModule::Upstreams)?;
    config.strategy = from_tree(&tree, ConfigModule::Strategy)?;
    config.hosts = from_tree(&tree, ConfigModule::Hosts)?;
    config.outbound = from_tree(&tree, ConfigModule::Outbound)?;
    config.rule_set = from_tree(&tree, ConfigModule::RuleSet)?;
    config.clients = from_tree(&tree, ConfigModule::Clients)?;
    config.dns = from_tree(&tree, ConfigModule::Dns)?;
    config.statistics = from_tree(&tree, ConfigModule::Statistics)?;
    config.logs = from_tree(&tree, ConfigModule::Logs)?;
    config.validate()?;
    Ok(AppliedChanges { config, renames })
}

fn to_value(value: &impl Serialize) -> Result<Value, EditError> {
    to_source_value(value).map_err(|_| EditError::Internal)
}

fn from_tree<T: DeserializeOwned>(tree: &Value, module: ConfigModule) -> Result<T, EditError> {
    yaml_serde::from_value(tree[module.key()].clone()).map_err(|_| EditError::Internal)
}

fn resource<'a, T: Serialize>(
    change: &'a ResourceMutation<T>,
    original: &[T],
    name: impl Fn(&T) -> &str,
) -> Result<(Option<&'a str>, Value), EditError> {
    match change {
        ResourceMutation::Create { value } => Ok((None, to_value(value)?)),
        ResourceMutation::Update {
            original_name,
            value,
        } => {
            original
                .iter()
                .find(|item| name(item) == original_name)
                .ok_or(EditError::NotFound)?;
            Ok((Some(original_name), to_value(value)?))
        }
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
                    .ok_or(EditError::Internal)?
                {
                    rename_field(route, "strategy", ConfigModule::Strategy, renames);
                }
            }
            _ => return Err(EditError::Internal),
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
            _ => return Err(EditError::Internal),
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
            .ok_or(EditError::Internal)?
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
