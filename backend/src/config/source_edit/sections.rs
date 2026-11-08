//! 顶层块映射的受限分段编辑，绕过 CST 对顶格列表后续同级键的错误归属。

use std::collections::BTreeSet;

use yaml_edit::SyntaxKind;
use yaml_serde::Value;

use super::render::{Style, block_entry};
use super::{Document, FromStr, SourceEditError, edit_with_cst, preferred_newline};

/// 分段只提供定位边界：每段必须独立解析为原树中的一个顶层项，不能靠文本形状猜测语义。
/// 不重排或重新序列化原文；顶层删除不属于管理变更契约，兼容路径仍拒绝它。
pub(super) fn edit_block_sections(
    source: &str,
    original: &Value,
    intended: &Value,
) -> Result<String, SourceEditError> {
    let old = original
        .as_mapping()
        .ok_or(SourceEditError::RootMustBeMapping)?;
    let new = intended
        .as_mapping()
        .ok_or(SourceEditError::RootMustBeMapping)?;
    let (indent, starts) = section_starts(source)?;
    if starts.len() != old.len() {
        return Err(SourceEditError::UnsupportedSyntax);
    }
    let mut seen = BTreeSet::new();
    let mut result = source[..starts[0]].to_owned();
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(source.len());
        let section = &source[start..end];
        let parsed: Value =
            yaml_serde::from_str(section).map_err(|_| SourceEditError::UnsupportedSyntax)?;
        let fields = parsed
            .as_mapping()
            .ok_or(SourceEditError::UnsupportedSyntax)?;
        if fields.len() != 1 {
            return Err(SourceEditError::UnsupportedSyntax);
        }
        let (key, value) = fields
            .iter()
            .next()
            .ok_or(SourceEditError::UnsupportedSyntax)?;
        let field = key.as_str().ok_or(SourceEditError::UnsupportedSyntax)?;
        if !seen.insert(field.to_owned()) || old.get(key) != Some(value) {
            return Err(SourceEditError::UnsupportedSyntax);
        }
        let next = new.get(key).ok_or(SourceEditError::UnsupportedSyntax)?;
        let mut edited = if value == next {
            section.to_owned()
        } else {
            let target = Value::Mapping([(key.clone(), next.clone())].into_iter().collect());
            edit_with_cst(section, &parsed, &target)?
        };
        if index + 1 == starts.len() {
            // 新模块写在最后一个映射项之后、显式文档结束标记之前，不改变已有项的原始表达。
            let additions = new
                .iter()
                .filter(|(key, _)| !old.contains_key(*key))
                .map(|(key, value)| {
                    block_entry(
                        key,
                        value,
                        Style {
                            indent,
                            ..Style::default()
                        },
                        preferred_newline(source),
                    )
                })
                .collect::<Result<Vec<_>, SourceEditError>>()?;
            if !additions.is_empty() {
                let document =
                    Document::from_str(&edited).map_err(|_| SourceEditError::InvalidDocument)?;
                let root = document
                    .as_mapping()
                    .ok_or(SourceEditError::RootMustBeMapping)?;
                let at = root.byte_range().end as usize;
                let prefix = if edited[..at].ends_with('\n') {
                    ""
                } else {
                    preferred_newline(source)
                };
                edited.insert_str(at, &format!("{prefix}{}", additions.concat()));
            }
        }
        result.push_str(&edited);
    }
    let verified: Value =
        yaml_serde::from_str(&result).map_err(|_| SourceEditError::UnsupportedSyntax)?;
    if verified != *intended {
        return Err(SourceEditError::UnsupportedSyntax);
    }
    Ok(result)
}

/// 使用词法 token 找行首字符串键，不按行正则切割多行引号字符串。
/// 候选边界仍须逐段语义复核；flow 内容或不支持的复杂键造成歧义时拒绝兼容路径。
fn section_starts(source: &str) -> Result<(usize, Vec<usize>), SourceEditError> {
    let tokens = yaml_edit::lex(source);
    let mut candidates = Vec::new();
    let mut offset = 0;
    let mut line_start = 0;
    for (index, &(kind, text)) in tokens.iter().enumerate() {
        if kind == SyntaxKind::STRING
            && source[line_start..offset].bytes().all(|byte| byte == b' ')
            && tokens[index + 1..]
                .iter()
                .find(|(kind, _)| *kind != SyntaxKind::WHITESPACE)
                .is_some_and(|(kind, _)| *kind == SyntaxKind::COLON)
        {
            candidates.push((offset - line_start, line_start));
        }
        if let Some(newline) = text.rfind('\n') {
            line_start = offset + newline + 1;
        }
        offset += text.len();
    }
    let indent = candidates
        .iter()
        .map(|(indent, _)| *indent)
        .min()
        .ok_or(SourceEditError::UnsupportedSyntax)?;
    let starts = candidates
        .into_iter()
        .filter_map(|(column, start)| (column == indent).then_some(start))
        .collect();
    Ok((indent, starts))
}
