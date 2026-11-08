//! 保留源文档格式的受限 YAML 编辑能力。

use std::str::FromStr;

use thiserror::Error;
use yaml_edit::Document;

use render::{Style, block_entry, block_value, is_block_value};

mod render;
mod sections;

#[cfg(test)]
mod sections_tests;

#[cfg(test)]
mod style_tests;

/// 根据已校验的源树差异修改 CST；输出必须与目标树完全等价，不能静默损坏不支持的语法。
pub(crate) fn edit_document(
    source: &str,
    original: &yaml_serde::Value,
    intended: &yaml_serde::Value,
) -> Result<String, SourceEditError> {
    use yaml_edit::SyntaxKind;
    // 锚点可能让一个局部编辑影响多个别名；本期明确拒绝，不猜测其写回语义。
    if yaml_edit::lex(source).iter().any(|(kind, _)| {
        matches!(
            kind,
            SyntaxKind::ANCHOR | SyntaxKind::REFERENCE | SyntaxKind::MERGE_KEY
        )
    }) {
        return Err(SourceEditError::UnsupportedSyntax);
    }
    match edit_with_cst(source, original, intended) {
        // yaml-edit 会把顶格列表之后的同级键归入前一个列表项；分段路径逐项核对语义后再编辑。
        Err(SourceEditError::UnsupportedSyntax) => {
            sections::edit_block_sections(source, original, intended)
        }
        result => result,
    }
}

fn edit_with_cst(
    source: &str,
    original: &yaml_serde::Value,
    intended: &yaml_serde::Value,
) -> Result<String, SourceEditError> {
    let document = Document::from_str(source).map_err(|_| SourceEditError::InvalidDocument)?;
    let root = document
        .as_mapping()
        .ok_or(SourceEditError::RootMustBeMapping)?;
    let mut edits = Vec::new();
    collect_edits(
        source,
        yaml_edit::YamlNode::Mapping(root),
        original,
        intended,
        Style::default(),
        &mut edits,
    )?;
    edits.sort_by_key(|edit| edit.0);
    if edits.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(SourceEditError::UnsupportedSyntax);
    }
    let mut result = source.to_owned();
    for (start, end, replacement) in edits.into_iter().rev() {
        result.replace_range(start..end, &replacement);
    }
    let verified: yaml_serde::Value =
        yaml_serde::from_str(&result).map_err(|_| SourceEditError::UnsupportedSyntax)?;
    if verified != *intended {
        return Err(SourceEditError::UnsupportedSyntax);
    }
    Ok(result)
}

/// CST 只用于定位，不调用会破坏嵌套节点的 remove/set；编辑范围必须互不重叠。
fn collect_edits(
    source: &str,
    node: yaml_edit::YamlNode,
    old: &yaml_serde::Value,
    new: &yaml_serde::Value,
    parent_style: Style,
    edits: &mut Vec<(usize, usize, String)>,
) -> Result<(), SourceEditError> {
    use yaml_edit::YamlNode;
    use yaml_serde::Value;
    if old == new {
        return Ok(());
    }
    let range = node_range(&node)?;
    let raw = &source[range.start as usize..range.end as usize];
    let style = parent_style.at(source, &node, old)?;
    let flow = style.flow;
    // 空的 flow 占位符在块式父节点下首次填充时展开，不能把 JSON 写法传播到新配置。
    if raw.trim_start().starts_with(['{', '[']) && !flow && is_block_value(new) {
        return replace_node(source, range, new, style, edits);
    }
    match (node, old, new) {
        (YamlNode::Mapping(mapping), Value::Mapping(old), Value::Mapping(new)) => {
            if new.is_empty() || (flow && old.keys().any(|key| !new.contains_key(key))) {
                replace_node(source, range, &Value::Mapping(new.clone()), style, edits)?;
                return Ok(());
            }
            for key in old.keys().filter(|key| !new.contains_key(*key)) {
                let field = key.as_str().ok_or(SourceEditError::UnsupportedSyntax)?;
                let entry = mapping
                    .find_all_entries_by_key(field)
                    .next()
                    .ok_or(SourceEditError::UnsupportedSyntax)?;
                let key_range =
                    node_range(&entry.key_node().ok_or(SourceEditError::UnsupportedSyntax)?)?;
                let value_range = node_range(
                    &entry
                        .value_node()
                        .ok_or(SourceEditError::UnsupportedSyntax)?,
                )?;
                let start = line_start(source, key_range.start as usize);
                if !source[start..key_range.start as usize].trim().is_empty() {
                    return Err(SourceEditError::UnsupportedSyntax);
                }
                let end = value_range.end as usize;
                let end = if source[..end].ends_with('\n') {
                    end
                } else {
                    source[end..]
                        .find('\n')
                        .map_or(source.len(), |offset| end + offset + 1)
                };
                edits.push((start, end, String::new()));
            }
            let mut additions = Vec::new();
            for (key, value) in new {
                let field = key.as_str().ok_or(SourceEditError::UnsupportedSyntax)?;
                if let Some(previous) = old.get(key) {
                    let child = mapping
                        .get(field)
                        .ok_or(SourceEditError::UnsupportedSyntax)?;
                    collect_edits(
                        source,
                        child,
                        previous,
                        value,
                        style.child(style.step),
                        edits,
                    )?;
                } else if flow {
                    additions.push(format!(
                        "{}: {}",
                        quote_yaml_string(field),
                        flow_value(value)?
                    ));
                } else {
                    additions.push(block_entry(key, value, style, preferred_newline(source))?);
                }
            }
            if !additions.is_empty() {
                if flow {
                    let end = range.start as usize
                        + raw.rfind('}').ok_or(SourceEditError::UnsupportedSyntax)?;
                    let prefix = flow_separator(&source[range.start as usize..end], old.is_empty());
                    edits.push((end, end, format!("{prefix}{}", additions.join(", "))));
                } else {
                    let newline = preferred_newline(source);
                    let prefix = if raw.ends_with('\n') { "" } else { newline };
                    let addition = additions.concat();
                    edits.push((
                        range.end as usize,
                        range.end as usize,
                        format!("{prefix}{addition}"),
                    ));
                }
            }
            Ok(())
        }
        (YamlNode::Sequence(sequence), Value::Sequence(old), Value::Sequence(new)) => {
            if new.len() < old.len() {
                replace_node(source, range, &Value::Sequence(new.clone()), style, edits)?;
                return Ok(());
            }
            for (index, (previous, value)) in old.iter().zip(new).enumerate() {
                let child = sequence
                    .get(index)
                    .ok_or(SourceEditError::UnsupportedSyntax)?;
                collect_edits(source, child, previous, value, style.child(2), edits)?;
            }
            if new.len() > old.len() {
                if flow {
                    let additions = new
                        .iter()
                        .skip(old.len())
                        .map(flow_value)
                        .collect::<Result<Vec<_>, _>>()?;
                    let end = range.start as usize
                        + raw.rfind(']').ok_or(SourceEditError::UnsupportedSyntax)?;
                    let prefix = flow_separator(&source[range.start as usize..end], old.is_empty());
                    edits.push((end, end, format!("{prefix}{}", additions.join(", "))));
                } else {
                    let newline = preferred_newline(source);
                    let prefix = if raw.ends_with('\n') { "" } else { newline };
                    let addition =
                        block_value(&Value::Sequence(new[old.len()..].to_vec()), style, newline)?;
                    edits.push((
                        range.end as usize,
                        range.end as usize,
                        format!("{prefix}{addition}"),
                    ));
                }
            }
            Ok(())
        }
        (_, _, new) => replace_node(source, range, new, style, edits),
    }
}

/// 忽略空白与注释后判断已有尾逗号；字符串内的逗号不能当作集合分隔符。
fn flow_separator(before_close: &str, empty: bool) -> &'static str {
    use yaml_edit::SyntaxKind;
    if empty {
        return "";
    }
    let last = yaml_edit::lex(before_close)
        .into_iter()
        .rev()
        .find(|(kind, _)| {
            !matches!(
                kind,
                SyntaxKind::WHITESPACE
                    | SyntaxKind::NEWLINE
                    | SyntaxKind::INDENT
                    | SyntaxKind::COMMENT
            )
        });
    if last.is_some_and(|(kind, _)| kind == SyntaxKind::COMMA) {
        " "
    } else {
        ", "
    }
}

fn node_range(node: &yaml_edit::YamlNode) -> Result<yaml_edit::TextPosition, SourceEditError> {
    match node {
        yaml_edit::YamlNode::Mapping(node) => Ok(node.byte_range()),
        yaml_edit::YamlNode::Sequence(node) => Ok(node.byte_range()),
        yaml_edit::YamlNode::Scalar(node) => Ok(node.byte_range()),
        _ => Err(SourceEditError::UnsupportedSyntax),
    }
}

fn line_start(source: &str, index: usize) -> usize {
    source[..index]
        .rfind('\n')
        .map_or(0, |position| position + 1)
}

fn replace_node(
    source: &str,
    range: yaml_edit::TextPosition,
    value: &yaml_serde::Value,
    style: Style,
    edits: &mut Vec<(usize, usize, String)>,
) -> Result<(), SourceEditError> {
    let mut start = range.start as usize;
    let mut end = range.end as usize;
    let raw = &source[start..end];
    let newline = preferred_newline(source);
    let ending = if raw.ends_with('\n') { newline } else { "" };
    let replacement = if style.flow || !is_block_value(value) {
        format!("{}{ending}", flow_value(value)?)
    } else {
        let mut rendered = String::new();
        if raw.trim_start().starts_with(['{', '[']) {
            // 空 flow 容器的内部注释不在语义树中；展开前移到新子块之前，避免等价校验漏掉文本损失。
            for (kind, text) in yaml_edit::lex(raw) {
                if kind == yaml_edit::SyntaxKind::COMMENT {
                    rendered.push_str(&" ".repeat(style.indent));
                    rendered.push_str(text.trim_end_matches(['\r', '\n']));
                    rendered.push_str(newline);
                }
            }
        }
        rendered.push_str(&block_value(value, style, newline)?);
        let prefix = &source[line_start(source, start)..start];
        if prefix.trim().is_empty() || prefix.trim() == "-" {
            // 原本就是块节点（或紧跟列表短横线），第一行的缩进已在替换区间之外。
            format!(
                "{}{ending}",
                rendered
                    .trim_start_matches(' ')
                    .strip_suffix(newline)
                    .unwrap_or(&rendered)
            )
        } else {
            // 将 `key: [] # 注释` 展开时把注释放回 key 行，不让它落到最后一个子字段。
            start -= prefix.len() - prefix.trim_end_matches([' ', '\t']).len();
            let line_end = if ending.is_empty() {
                source[end..]
                    .find(['\r', '\n'])
                    .map_or(source.len(), |offset| end + offset)
            } else {
                end
            };
            let comment = &source[end..line_end];
            if !comment.trim().is_empty() && !comment.trim_start().starts_with('#') {
                return Err(SourceEditError::UnsupportedSyntax);
            }
            end = line_end;
            format!(
                "{comment}{newline}{}{ending}",
                rendered.strip_suffix(newline).unwrap_or(&rendered)
            )
        }
    };
    edits.push((start, end, replacement));
    Ok(())
}

fn flow_value(value: &impl serde::Serialize) -> Result<String, SourceEditError> {
    // serializer 负责转义；保留 flow 分隔空白，使下一次 CST 解析仍然可编辑。
    Ok(serde_json::to_string_pretty(value)
        .map_err(|_| SourceEditError::UnsupportedSyntax)?
        .replace('\n', " "))
}

/// 首次初始化写入源配置所需的最小用户投影。
pub(crate) struct InitialWebUiUser<'a> {
    pub(crate) name: &'a str,
    pub(crate) password_hash: &'a str,
}

/// 在源 YAML 中创建唯一的首个 WebUI 用户，并保留未修改区域的原始文本。
pub(crate) fn create_initial_webui_user(
    source: &str,
    user: InitialWebUiUser<'_>,
) -> Result<String, SourceEditError> {
    let document = Document::from_str(source).map_err(|_| SourceEditError::InvalidDocument)?;
    let root = document
        .as_mapping()
        .ok_or(SourceEditError::RootMustBeMapping)?;
    if root.find_all_entries_by_key("webui").count() != 1 {
        return Err(SourceEditError::AmbiguousWebUi);
    }
    let webui = root
        .get_mapping("webui")
        .ok_or(SourceEditError::WebUiMustBeMapping)?;
    if webui.find_all_entries_by_key("users").count() > 1 {
        return Err(SourceEditError::AmbiguousUsers);
    }
    if let Some(users) = webui.get_sequence("users") {
        if !users.is_empty() {
            return Err(SourceEditError::AlreadyInitialized);
        }
        let range = users.byte_range();
        return replace_empty_users_sequence(
            source,
            range.start as usize,
            range.end as usize,
            user,
        );
    } else if webui.contains_key("users") {
        return Err(SourceEditError::UsersMustBeSequence);
    }

    let range = webui.byte_range();
    insert_missing_users(source, range.start as usize, range.end as usize, user)
}

fn replace_empty_users_sequence(
    source: &str,
    sequence_start: usize,
    sequence_end: usize,
    user: InitialWebUiUser<'_>,
) -> Result<String, SourceEditError> {
    let line_start = source[..sequence_start]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let indent = source[line_start..sequence_start]
        .chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .collect::<String>();
    let value_start = source[line_start..sequence_start]
        .trim_end_matches([' ', '\t'])
        .len()
        + line_start;
    let line_end = source[sequence_end..]
        .find(['\r', '\n'])
        .map_or(source.len(), |offset| sequence_end + offset);
    let has_inline_comment = source[sequence_end..line_end].trim_start().starts_with('#');
    let newline = preferred_newline(source);

    let mut replacement = render_users_value(indent.as_str(), newline, user);
    if has_inline_comment {
        replacement.push_str(newline);
        replacement.push_str(&indent);
    }

    let mut updated = source.to_owned();
    updated.replace_range(value_start..sequence_end, &replacement);
    Ok(updated)
}

fn insert_missing_users(
    source: &str,
    mapping_start: usize,
    mapping_end: usize,
    user: InitialWebUiUser<'_>,
) -> Result<String, SourceEditError> {
    let line_start = source[..mapping_start]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let indent = source[line_start..mapping_start]
        .chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .collect::<String>();
    let newline = preferred_newline(source);
    let prefix = if source[..mapping_end].ends_with(['\r', '\n']) {
        ""
    } else {
        newline
    };
    let suffix = if source[mapping_end..].starts_with(['\r', '\n']) {
        ""
    } else {
        newline
    };
    let value = render_users_value(indent.as_str(), newline, user);
    let insertion = format!("{prefix}{indent}users:{value}{suffix}");

    let mut updated = source.to_owned();
    updated.insert_str(mapping_end, &insertion);
    Ok(updated)
}

fn render_users_value(indent: &str, newline: &str, user: InitialWebUiUser<'_>) -> String {
    format!(
        "{newline}{indent}  - name: {}{newline}{indent}    password_hash: {}",
        quote_yaml_string(user.name),
        quote_yaml_string(user.password_hash)
    )
}

fn quote_yaml_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn preferred_newline(source: &str) -> &'static str {
    if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum SourceEditError {
    #[error("source syntax cannot be edited without changing unrelated semantics")]
    UnsupportedSyntax,
    #[error("source configuration is not valid YAML")]
    InvalidDocument,
    #[error("source configuration root must be a mapping")]
    RootMustBeMapping,
    #[error("source configuration must contain exactly one webui mapping")]
    AmbiguousWebUi,
    #[error("source configuration webui value must be a mapping")]
    WebUiMustBeMapping,
    #[error("source configuration must not contain duplicate webui.users fields")]
    AmbiguousUsers,
    #[error("source configuration webui.users must be a sequence")]
    UsersMustBeSequence,
    #[error("webui already has at least one configured user")]
    AlreadyInitialized,
}

#[cfg(test)]
mod tests {
    use super::{InitialWebUiUser, SourceEditError, create_initial_webui_user};

    const HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$2M7ZV4yI1YVh7VdXk9G97A";

    #[test]
    fn creates_user_without_rewriting_unrelated_source() {
        let source = "# top comment\nversion: 1\nwebui:\n  enable: true\n  address: 127.0.0.1\n  port: 8080\n  public_origin: http://127.0.0.1:8080\n  # keep this comment\n  users: []\noutbound:\n  proxy_url: env:PROXY_URL\n";
        let updated = create_initial_webui_user(
            source,
            InitialWebUiUser {
                name: "admin",
                password_hash: HASH,
            },
        )
        .unwrap();

        assert!(updated.contains("# top comment"), "{updated}");
        assert!(updated.contains("# keep this comment"), "{updated}");
        assert!(updated.contains("proxy_url: env:PROXY_URL"), "{updated}");
        assert!(updated.contains("name: 'admin'"), "{updated}");
        assert!(updated.contains(HASH), "{updated}");
    }

    #[test]
    fn rejects_non_empty_or_ambiguous_users() {
        let non_empty = "webui:\n  users:\n    - name: existing\n      password_hash: hash\n";
        assert_eq!(
            create_initial_webui_user(
                non_empty,
                InitialWebUiUser {
                    name: "admin",
                    password_hash: HASH,
                },
            ),
            Err(SourceEditError::AlreadyInitialized)
        );

        let duplicate = "webui:\n  users: []\n  users: []\n";
        assert_eq!(
            create_initial_webui_user(
                duplicate,
                InitialWebUiUser {
                    name: "admin",
                    password_hash: HASH,
                },
            ),
            Err(SourceEditError::AmbiguousUsers)
        );
    }

    #[test]
    fn creates_omitted_users_field_inside_webui_mapping() {
        let source = "version: 1\nwebui:\n    enable: true\n    port: 8080\ndns:\n  cache: false\n";
        let updated = create_initial_webui_user(
            source,
            InitialWebUiUser {
                name: "admin",
                password_hash: HASH,
            },
        )
        .unwrap();

        assert!(
            updated.contains("    users:\n      - name: 'admin'\n        password_hash:"),
            "{updated}"
        );
        assert!(updated.contains("dns:\n  cache: false"), "{updated}");
    }
}
