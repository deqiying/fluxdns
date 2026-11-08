//! 只渲染本次新增或必须替换的片段，不对未修改的源区域做格式化。

use yaml_edit::YamlNode;
use yaml_serde::Value;

use super::{SourceEditError, flow_value, line_start, node_range};

#[derive(Clone, Copy)]
pub(super) struct Style {
    pub(super) flow: bool,
    pub(super) indent: usize,
    pub(super) step: usize,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            flow: false,
            indent: 0,
            step: 2,
        }
    }
}

impl Style {
    /// 非空 flow 容器沿用原风格；空容器没有可参考的布局，继承父节点。
    /// 缩进步长优先从当前块映射的子容器推断，再继承上层，根节点默认两格。
    pub(super) fn at(
        self,
        source: &str,
        node: &YamlNode,
        old: &Value,
    ) -> Result<Self, SourceEditError> {
        let range = node_range(node)?;
        let raw = &source[range.start as usize..range.end as usize];
        let inline = raw.trim_start().starts_with(['{', '[']);
        let mut style = Self {
            flow: self.flow || (inline && is_block_value(old)),
            indent: if !inline && is_block_value(old) {
                range.start as usize - line_start(source, range.start as usize)
            } else {
                self.indent
            },
            ..self
        };
        if !style.flow
            && let (YamlNode::Mapping(mapping), Value::Mapping(fields)) = (node, old)
        {
            style.step = fields
                .keys()
                .filter_map(|key| {
                    let child = mapping.get(key.as_str()?)?;
                    if !matches!(child, YamlNode::Mapping(_) | YamlNode::Sequence(_)) {
                        return None;
                    }
                    let range = node_range(&child).ok()?;
                    let start = range.start as usize;
                    let prefix = &source[line_start(source, start)..start];
                    if !prefix.bytes().all(|byte| byte == b' ') {
                        return None;
                    }
                    prefix
                        .len()
                        .checked_sub(style.indent)
                        .filter(|step| *step > 0)
                })
                .min()
                .unwrap_or(self.step);
        }
        Ok(style)
    }

    pub(super) fn child(self, offset: usize) -> Self {
        Self {
            indent: self.indent + offset,
            ..self
        }
    }
}

pub(super) fn is_block_value(value: &Value) -> bool {
    match value {
        Value::Mapping(mapping) => !mapping.is_empty(),
        Value::Sequence(sequence) => !sequence.is_empty(),
        _ => false,
    }
}

/// YAML serializer 决定 plain/quoted 标量；多行内容使用转义字符串，避免改变尾部换行语义。
fn scalar_value(value: &Value) -> Result<String, SourceEditError> {
    if value
        .as_str()
        .is_some_and(|text| text.contains(['\n', '\r']))
    {
        return flow_value(value);
    }
    let yaml = yaml_serde::to_string(value).map_err(|_| SourceEditError::UnsupportedSyntax)?;
    let scalar = yaml.trim_end_matches('\n');
    if scalar.contains(['\n', '\r']) {
        flow_value(value)
    } else {
        Ok(scalar.to_owned())
    }
}

pub(super) fn block_entry(
    key: &Value,
    value: &Value,
    style: Style,
    newline: &str,
) -> Result<String, SourceEditError> {
    if !key.is_string() {
        return Err(SourceEditError::UnsupportedSyntax);
    }
    let prefix = format!("{}{}:", " ".repeat(style.indent), scalar_value(key)?);
    if is_block_value(value) {
        Ok(format!(
            "{prefix}{newline}{}",
            block_value(value, style.child(style.step), newline)?
        ))
    } else {
        Ok(format!("{prefix} {}{newline}", scalar_value(value)?))
    }
}

/// 列表项的首行紧随 `- `，后续映射键对齐首个键；嵌套容器使用继承的缩进步长。
/// 空集合保留 `{}` / `[]`，不能用空块表达，否则 YAML 会把它解析成 null。
pub(super) fn block_value(
    value: &Value,
    style: Style,
    newline: &str,
) -> Result<String, SourceEditError> {
    match value {
        Value::Mapping(mapping) if !mapping.is_empty() => mapping
            .iter()
            .map(|(key, value)| block_entry(key, value, style, newline))
            .collect(),
        Value::Sequence(sequence) if !sequence.is_empty() => sequence
            .iter()
            .map(|value| {
                let child = block_value(value, style.child(2), newline)?;
                Ok(format!(
                    "{}- {}",
                    " ".repeat(style.indent),
                    &child[style.indent + 2..]
                ))
            })
            .collect(),
        _ => Ok(format!(
            "{}{}{newline}",
            " ".repeat(style.indent),
            scalar_value(value)?
        )),
    }
}
