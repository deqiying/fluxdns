//! 条目级写回：只替换源文件中发生变化的条目（列表项、单例段子键或整段），其余文本逐字节不变。
//!
//! 定位交给 [`layout`]，并要求每个切片单独解析后与整份文档的解析值一致（自证）；对不上时
//! 降级为整段替换。替换后的全文必须重新严格解析，且源序列化值与目标配置完全相等，否则
//! 返回 [`WriteError::Layout`]，调用方据此拒绝写入。这是写回层唯一的安全检查。

use std::collections::BTreeMap;

use thiserror::Error;
use yaml_serde::{Mapping, Value};

use super::change::{ConfigModule, Renames};
use super::contract::ConfigV2;
use super::layout::{self, DocumentLayout, Section, Span};
use super::model::to_source_value;

/// 配置编辑器可以修改的顶层模块；其余顶层键（`version`、`work`、`database`、`webui`）
/// 由写回层原样保留。
pub(crate) const EDITABLE_MODULES: [ConfigModule; 10] = [
    ConfigModule::Listener,
    ConfigModule::Upstreams,
    ConfigModule::Strategy,
    ConfigModule::Hosts,
    ConfigModule::Outbound,
    ConfigModule::RuleSet,
    ConfigModule::Clients,
    ConfigModule::Dns,
    ConfigModule::Statistics,
    ConfigModule::Logs,
];

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum WriteError {
    /// 源文件布局无法安全修改（例如条目内含锚点、别名），或写回结果未通过语义复核。
    #[error("configuration source layout of {module} cannot be edited safely")]
    Layout {
        module: &'static str,
        name: Option<String>,
    },
    /// 类型化配置无法序列化为 YAML 值；正常 DTO 不会出现，出现即视为内部错误。
    #[error("configuration value cannot be serialized")]
    Serialize,
}

/// 各可编辑模块的源序列化值（不脱敏），用于差异比较、渲染和复核。
pub(crate) type SourceModel = BTreeMap<ConfigModule, Value>;

pub(crate) fn source_model(config: &ConfigV2) -> Result<SourceModel, WriteError> {
    let mut model = SourceModel::new();
    for module in EDITABLE_MODULES {
        let value = match module {
            ConfigModule::Listener => to_source_value(&config.listener),
            ConfigModule::Upstreams => to_source_value(&config.upstreams),
            ConfigModule::Strategy => to_source_value(&config.strategy),
            ConfigModule::Hosts => to_source_value(&config.hosts),
            ConfigModule::Outbound => to_source_value(&config.outbound),
            ConfigModule::RuleSet => to_source_value(&config.rule_set),
            ConfigModule::Clients => to_source_value(&config.clients),
            ConfigModule::Dns => to_source_value(&config.dns),
            ConfigModule::Statistics => to_source_value(&config.statistics),
            ConfigModule::Logs => to_source_value(&config.logs),
        }
        .map_err(|_| WriteError::Serialize)?;
        model.insert(module, value);
    }
    Ok(model)
}

pub(crate) fn is_list_module(module: ConfigModule) -> bool {
    !matches!(
        module,
        ConfigModule::Dns | ConfigModule::Statistics | ConfigModule::Logs
    )
}

/// 把 `base`（即 `text` 的解析结果）到 `next` 的差异写回 `text`。
///
/// `renames` 是本次改名的旧名到新名映射，用于把列表项按名称对应起来。没有差异时原样返回。
pub(crate) fn render(
    text: &str,
    base: &ConfigV2,
    next: &ConfigV2,
    renames: &Renames,
) -> Result<String, WriteError> {
    let base_model = source_model(base)?;
    let next_model = source_model(next)?;
    let changed: Vec<ConfigModule> = EDITABLE_MODULES
        .into_iter()
        .filter(|module| base_model[module] != next_model[module])
        .collect();
    let Some(first) = changed.first().copied() else {
        return Ok(text.to_owned());
    };
    let mut writer = Writer {
        text,
        crlf: text.contains("\r\n"),
        edits: Vec::new(),
        target: None,
    };
    let raw: Value = yaml_serde::from_str(text).map_err(|_| writer.failure(first))?;
    let layout = layout::scan_document(text).filter(|layout| keys_match(layout, &raw));
    let output = match layout {
        Some(layout) => {
            for module in &changed {
                writer.section(
                    &layout,
                    &raw,
                    *module,
                    &base_model[module],
                    &next_model[module],
                    renames,
                )?;
            }
            writer.apply()
        }
        None => writer.whole_document(&raw, &changed, &next_model)?,
    };
    if !reviewed(&output, &raw, &next_model) {
        return Err(writer.failure(first));
    }
    Ok(output)
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

struct Writer<'a> {
    text: &'a str,
    crlf: bool,
    edits: Vec<Edit>,
    /// 第一个被改写的条目，失败时用于诊断定位。
    target: Option<(ConfigModule, Option<String>)>,
}

impl Writer<'_> {
    fn note(&mut self, module: ConfigModule, name: Option<&str>) {
        if self.target.is_none() {
            self.target = Some((module, name.map(str::to_owned)));
        }
    }

    fn failure(&self, fallback: ConfigModule) -> WriteError {
        let (module, name) = self.target.clone().unwrap_or((fallback, None));
        WriteError::Layout {
            module: module.key(),
            name,
        }
    }

    fn newline(&self, fragment: String) -> String {
        if self.crlf {
            fragment.replace('\n', "\r\n")
        } else {
            fragment
        }
    }

    /// 插入点前一行没有换行符（文件末尾缺少换行）时补一个，避免新内容接在旧行尾。
    fn insertion(&self, at: usize, fragment: String) -> Edit {
        let prefix = if at > 0 && !self.text[..at].ends_with('\n') {
            "\n"
        } else {
            ""
        };
        Edit {
            start: at,
            end: at,
            text: self.newline(format!("{prefix}{fragment}")),
        }
    }

    /// 替换已有区间；原区间没有末尾换行（位于文件末尾）时，替换内容也不追加换行。
    fn replacement(&self, span: Span, fragment: String) -> Edit {
        let original = &self.text[span.start..span.end];
        let fragment = if original.ends_with('\n') {
            fragment
        } else {
            fragment.trim_end_matches('\n').to_owned()
        };
        Edit {
            start: span.start,
            end: span.end,
            text: self.newline(fragment),
        }
    }

    /// 被替换或删除的切片含锚点/别名时明确拒绝：重写会丢失锚点或展开别名，改变其他位置的语义。
    fn guard(
        &self,
        span: Span,
        module: ConfigModule,
        name: Option<&str>,
    ) -> Result<(), WriteError> {
        if layout::contains_anchor_syntax(&self.text[span.start..span.end]) {
            return Err(WriteError::Layout {
                module: module.key(),
                name: name.map(str::to_owned),
            });
        }
        Ok(())
    }

    fn section(
        &mut self,
        layout: &DocumentLayout,
        raw: &Value,
        module: ConfigModule,
        base: &Value,
        next: &Value,
        renames: &Renames,
    ) -> Result<(), WriteError> {
        let key = module.key();
        let Some(section) = layout.sections.iter().find(|section| section.key == key) else {
            self.note(module, None);
            let edit = self.insertion(layout.tail, render_section(key, next)?);
            self.edits.push(edit);
            return Ok(());
        };
        let raw_value = raw.get(key).unwrap_or(&Value::Null);
        let handled = if is_list_module(module) {
            self.items(section, raw_value, module, base, next, renames)?
        } else {
            self.keys(section, raw_value, module, base, next)?
        };
        if !handled {
            // 段内定位无法自证（flow 写法、单行多键等）：整段替换，段内注释会丢失。
            self.guard(section.span, module, None)?;
            self.note(module, None);
            let edit = self.replacement(section.span, render_section(key, next)?);
            self.edits.push(edit);
        }
        Ok(())
    }

    /// 列表段按 `name` 对应条目：相等的不动，不同的替换，缺失的删除，新增的追加到最后一项之后。
    /// 返回 `false` 表示需要整段替换。
    fn items(
        &mut self,
        section: &Section,
        raw: &Value,
        module: ConfigModule,
        base: &Value,
        next: &Value,
        renames: &Renames,
    ) -> Result<bool, WriteError> {
        let (Some(raw_items), Some(base_items), Some(next_items)) =
            (raw.as_sequence(), base.as_sequence(), next.as_sequence())
        else {
            return Ok(false);
        };
        let Some((indent, spans)) = layout::scan_items(self.text, section) else {
            return Ok(false);
        };
        let proven = spans.len() == raw_items.len()
            && base_items.len() == spans.len()
            && spans.iter().zip(raw_items).all(|(span, item)| {
                proves(
                    &self.text[span.start..span.end],
                    &Value::Sequence(vec![item.clone()]),
                )
            });
        if !proven {
            return Ok(false);
        }
        let mut plan = Vec::with_capacity(base_items.len());
        for item in base_items {
            let Some(name) = item_name(item) else {
                return Ok(false);
            };
            let target = renames
                .get(&(module, name.to_owned()))
                .map_or(name, String::as_str);
            plan.push(
                next_items
                    .iter()
                    .position(|candidate| item_name(candidate) == Some(target)),
            );
        }
        // 保留下来的条目必须保持原顺序并位于新增条目之前；其他顺序变化走整段替换。
        let kept: Vec<usize> = plan.iter().flatten().copied().collect();
        if kept.iter().enumerate().any(|(index, next)| index != *next) {
            return Ok(false);
        }
        let mut edits = Vec::new();
        for (index, target) in plan.iter().enumerate() {
            let span = spans[index];
            match target {
                None => {
                    let name = item_name(&base_items[index]);
                    self.guard(span, module, name)?;
                    self.note(module, name);
                    edits.push(self.replacement(span, String::new()));
                }
                Some(next_index) if base_items[index] != next_items[*next_index] => {
                    let name = item_name(&next_items[*next_index]);
                    self.guard(span, module, name)?;
                    self.note(module, name);
                    edits.push(
                        self.replacement(span, render_item(&next_items[*next_index], indent)?),
                    );
                }
                Some(_) => {}
            }
        }
        let mut appended = String::new();
        for item in &next_items[kept.len()..] {
            self.note(module, item_name(item));
            appended.push_str(&render_item(item, indent)?);
        }
        if !appended.is_empty() {
            let at = spans.last().map_or(section.span.end, |span| span.end);
            edits.push(self.insertion(at, appended));
        }
        self.edits.extend(edits);
        Ok(true)
    }

    /// 单例段按子键比较：值不同则替换，新出现的追加到最后一个子键之后，消失的删除。
    /// 返回 `false` 表示需要整段替换。
    fn keys(
        &mut self,
        section: &Section,
        raw: &Value,
        module: ConfigModule,
        base: &Value,
        next: &Value,
    ) -> Result<bool, WriteError> {
        let (Some(raw_map), Some(base_map), Some(next_map)) =
            (raw.as_mapping(), base.as_mapping(), next.as_mapping())
        else {
            return Ok(false);
        };
        // 删空后只能写成 `{}`，交给整段替换。
        if next_map.is_empty() {
            return Ok(false);
        }
        let Some((indent, keys)) = layout::scan_keys(self.text, section) else {
            return Ok(false);
        };
        if !proves_keys(self.text, &keys, raw_map) {
            return Ok(false);
        }
        let span_of = |key: &Value| {
            key.as_str()
                .and_then(|key| keys.iter().find(|(name, _)| name == key))
                .map(|(_, span)| *span)
        };
        let mut edits = Vec::new();
        let mut appended = String::new();
        for (key, value) in next_map {
            if base_map.get(key) == Some(value) {
                continue;
            }
            match span_of(key) {
                Some(span) => {
                    self.guard(span, module, None)?;
                    edits.push(self.replacement(span, render_entry(key, value, indent)?));
                }
                None => appended.push_str(&render_entry(key, value, indent)?),
            }
        }
        for key in base_map.keys().filter(|key| !next_map.contains_key(*key)) {
            if let Some(span) = span_of(key) {
                self.guard(span, module, None)?;
                edits.push(self.replacement(span, String::new()));
            }
        }
        if !appended.is_empty() {
            let at = keys.last().map_or(section.span.end, |(_, span)| span.end);
            edits.push(self.insertion(at, appended));
        }
        self.note(module, None);
        self.edits.extend(edits);
        Ok(true)
    }

    /// 根节点不是可识别的块式映射（例如整份 flow 文档）时，按解析值重写整份文档；注释全部丢失。
    fn whole_document(
        &mut self,
        raw: &Value,
        changed: &[ConfigModule],
        next: &SourceModel,
    ) -> Result<String, WriteError> {
        let first = changed[0];
        if layout::contains_anchor_syntax(self.text) {
            return Err(self.failure(first));
        }
        let mut tree = raw
            .as_mapping()
            .cloned()
            .ok_or_else(|| self.failure(first))?;
        for module in changed {
            tree.insert(Value::String(module.key().into()), next[module].clone());
        }
        let mut output = String::new();
        for (key, value) in &tree {
            let key = key.as_str().ok_or_else(|| self.failure(first))?;
            output.push_str(&render_section(key, value)?);
        }
        self.note(first, None);
        Ok(self.newline(output))
    }

    fn apply(&mut self) -> String {
        self.edits.sort_by_key(|edit| (edit.start, edit.end));
        let mut output = String::with_capacity(self.text.len());
        let mut cursor = 0;
        for edit in &self.edits {
            debug_assert!(edit.start >= cursor, "写回区间不能重叠");
            output.push_str(&self.text[cursor..edit.start]);
            output.push_str(&edit.text);
            cursor = edit.end;
        }
        output.push_str(&self.text[cursor..]);
        output
    }
}

/// 首个 WebUI 用户写回：只在 `webui` 段内写入 `users` 子键，其余文本逐字节不变。
///
/// 段内子键无法自证时降级为整段替换（段内注释丢失）。结果必须严格解析，且与原文相比只有
/// `webui.users` 变化，否则返回 [`WriteError::Layout`]，调用方据此拒绝写入。
pub(crate) fn set_webui_users(text: &str, users: &Value) -> Result<String, WriteError> {
    let failure = || WriteError::Layout {
        module: "webui",
        name: None,
    };
    let raw: Value = yaml_serde::from_str(text).map_err(|_| failure())?;
    let raw_webui = raw
        .get("webui")
        .and_then(Value::as_mapping)
        .ok_or_else(failure)?;
    let mut next_webui = raw_webui.clone();
    next_webui.insert(Value::String("users".into()), users.clone());
    let layout = layout::scan_document(text)
        .filter(|layout| keys_match(layout, &raw))
        .ok_or_else(failure)?;
    let section = layout
        .sections
        .iter()
        .find(|section| section.key == "webui")
        .ok_or_else(failure)?;
    let mut writer = Writer {
        text,
        crlf: text.contains("\r\n"),
        edits: Vec::new(),
        target: None,
    };
    let users_key = Value::String("users".into());
    let located =
        layout::scan_keys(text, section).filter(|(_, keys)| proves_keys(text, keys, raw_webui));
    let edit = match located {
        Some((indent, keys)) => {
            let entry = render_entry(&users_key, users, indent)?;
            match keys.iter().find(|(key, _)| key == "users") {
                Some((_, span)) => {
                    reject_anchor(text, *span)?;
                    writer.replacement(*span, entry)
                }
                None => {
                    let at = keys.last().map_or(section.span.end, |(_, span)| span.end);
                    writer.insertion(at, entry)
                }
            }
        }
        None => {
            reject_anchor(text, section.span)?;
            writer.replacement(
                section.span,
                render_section("webui", &Value::Mapping(next_webui.clone()))?,
            )
        }
    };
    writer.edits.push(edit);
    let output = writer.apply();
    let mut expected = raw.clone();
    expected
        .as_mapping_mut()
        .ok_or_else(failure)?
        .insert(Value::String("webui".into()), Value::Mapping(next_webui));
    let after: Value = yaml_serde::from_str(&output).map_err(|_| failure())?;
    if after != expected || ConfigV2::parse(output.as_bytes()).is_err() {
        return Err(failure());
    }
    Ok(output)
}

fn reject_anchor(text: &str, span: Span) -> Result<(), WriteError> {
    if layout::contains_anchor_syntax(&text[span.start..span.end]) {
        return Err(WriteError::Layout {
            module: "webui",
            name: None,
        });
    }
    Ok(())
}

fn keys_match(layout: &DocumentLayout, raw: &Value) -> bool {
    let Some(mapping) = raw.as_mapping() else {
        return false;
    };
    layout.sections.len() == mapping.len()
        && layout
            .sections
            .iter()
            .zip(mapping.keys())
            .all(|(section, key)| key.as_str() == Some(section.key.as_str()))
}

/// 单例段的子键区间必须与解析结果一一对应，且每个切片单独解析后等于对应的键值。
fn proves_keys(text: &str, keys: &[(String, Span)], raw: &Mapping) -> bool {
    keys.len() == raw.len()
        && keys
            .iter()
            .zip(raw)
            .all(|((key, span), (raw_key, raw_value))| {
                raw_key.as_str() == Some(key.as_str()) && {
                    let mut expected = Mapping::new();
                    expected.insert(raw_key.clone(), raw_value.clone());
                    proves(&text[span.start..span.end], &Value::Mapping(expected))
                }
            })
}

/// 切片单独解析后必须与整份文档中对应位置的值相等，才能证明定位正确。
fn proves(slice: &str, expected: &Value) -> bool {
    yaml_serde::from_str::<Value>(slice).is_ok_and(|value| value == *expected)
}

fn item_name(item: &Value) -> Option<&str> {
    item.get("name").and_then(Value::as_str)
}

/// 语义复核：结果必须通过严格解析，可编辑模块的源序列化值与目标一致，其余顶层键保持原值。
fn reviewed(output: &str, before: &Value, next: &SourceModel) -> bool {
    let Ok(parsed) = ConfigV2::parse(output.as_bytes()) else {
        return false;
    };
    if source_model(&parsed).ok().as_ref() != Some(next) {
        return false;
    }
    let Ok(after) = yaml_serde::from_str::<Value>(output) else {
        return false;
    };
    let others = |value: &Value| -> Option<Mapping> {
        let mapping = value.as_mapping()?;
        Some(
            mapping
                .iter()
                .filter(|(key, _)| {
                    !EDITABLE_MODULES
                        .iter()
                        .any(|module| key.as_str() == Some(module.key()))
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
    };
    others(before).is_some() && others(before) == others(&after)
}

const STEP: usize = 2;

fn is_block(value: &Value) -> bool {
    match value {
        Value::Mapping(mapping) => !mapping.is_empty(),
        Value::Sequence(sequence) => !sequence.is_empty(),
        _ => false,
    }
}

/// 渲染整段：`key:` 加两格缩进的块式正文；空集合与标量写在同一行。
fn render_section(key: &str, value: &Value) -> Result<String, WriteError> {
    render_entry(&Value::String(key.to_owned()), value, 0)
}

/// 渲染一个列表项：块式正文的首行缩进替换为原项缩进加 `- `。
///
/// 带标签的 DTO（`type` 区分变体）序列化时 `type` 在前；资源条目按配置文件惯例把 `name`
/// 放到首行，便于辨认。键顺序不影响语义。
fn render_item(value: &Value, indent: usize) -> Result<String, WriteError> {
    let reordered;
    let value = match value.as_mapping() {
        Some(mapping) if mapping.contains_key("name") => {
            let mut ordered = Mapping::new();
            ordered.insert(Value::String("name".into()), mapping["name"].clone());
            for (key, field) in mapping {
                if key.as_str() != Some("name") {
                    ordered.insert(key.clone(), field.clone());
                }
            }
            reordered = Value::Mapping(ordered);
            &reordered
        }
        _ => value,
    };
    let body = render_block(value, indent + STEP)?;
    Ok(format!(
        "{}- {}",
        " ".repeat(indent),
        &body[indent + STEP..]
    ))
}

fn render_entry(key: &Value, value: &Value, indent: usize) -> Result<String, WriteError> {
    let pad = " ".repeat(indent);
    let key = scalar(key)?;
    if is_block(value) {
        return Ok(format!(
            "{pad}{key}:\n{}",
            render_block(value, indent + STEP)?
        ));
    }
    if let Some(literal) = literal_block(value, indent + STEP) {
        return Ok(format!("{pad}{key}: {literal}"));
    }
    Ok(format!("{pad}{key}: {}\n", inline(value)?))
}

fn render_block(value: &Value, indent: usize) -> Result<String, WriteError> {
    match value {
        Value::Mapping(mapping) if !mapping.is_empty() => mapping
            .iter()
            .map(|(key, value)| render_entry(key, value, indent))
            .collect(),
        Value::Sequence(sequence) if !sequence.is_empty() => sequence
            .iter()
            .map(|item| render_item(item, indent))
            .collect(),
        _ => Ok(format!("{}{}\n", " ".repeat(indent), inline(value)?)),
    }
}

/// 空集合保留 `{}`/`[]`（空块会被解析成 null），其余为单行标量。
fn inline(value: &Value) -> Result<String, WriteError> {
    match value {
        Value::Mapping(mapping) if mapping.is_empty() => Ok("{}".into()),
        Value::Sequence(sequence) if sequence.is_empty() => Ok("[]".into()),
        _ => scalar(value),
    }
}

/// 引号与转义交给 serializer；含换行的字符串用 JSON 双引号形式，保证逐字等价。
fn scalar(value: &Value) -> Result<String, WriteError> {
    if value
        .as_str()
        .is_some_and(|text| text.contains(['\n', '\r']))
    {
        return serde_json::to_string(value).map_err(|_| WriteError::Serialize);
    }
    let yaml = yaml_serde::to_string(value).map_err(|_| WriteError::Serialize)?;
    let scalar = yaml.trim_end_matches('\n');
    if scalar.contains('\n') {
        return serde_json::to_string(value).map_err(|_| WriteError::Serialize);
    }
    Ok(scalar.to_owned())
}

/// 多行文本（如内联 hosts 规则）优先写成可读的字面块 `|`/`|-`。
///
/// 只处理可以无歧义往返的情形：不含 `\r` 和其他控制字符、末尾最多一个换行、首个非空行
/// 不以空白开头（否则需要缩进指示符）。其余情况返回 `None`，改用转义字符串。
fn literal_block(value: &Value, indent: usize) -> Option<String> {
    let text = value.as_str()?;
    if !text.contains('\n')
        || text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return None;
    }
    let (indicator, body) = match text.strip_suffix('\n') {
        Some(body) if body.ends_with('\n') => return None,
        Some(body) => ("|", body),
        None => ("|-", text),
    };
    let first = body.lines().find(|line| !line.is_empty())?;
    if first.starts_with([' ', '\t']) {
        return None;
    }
    let pad = " ".repeat(indent);
    let mut output = format!("{indicator}\n");
    for line in body.split('\n') {
        if !line.is_empty() {
            output.push_str(&pad);
            output.push_str(line);
        }
        output.push('\n');
    }
    Some(output)
}

#[cfg(test)]
mod tests;
