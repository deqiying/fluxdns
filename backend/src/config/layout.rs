//! 按行扫描 YAML 源文本布局，定位顶层段、段内列表项和段内子键的字节区间。
//!
//! 这里只识别配置文件常见的块式写法，不实现完整 YAML 语法：定位结果必须由调用方
//! （`writer`）把每个切片单独解析后与整份文档的解析值逐一比对，对不上就降级为整段替换。
//! 因此本模块宁可返回 `None`，也不猜测少见布局。

/// 源文本中的一行；偏移均为字节下标。
#[derive(Clone, Copy, Debug)]
struct Line {
    /// 行首偏移。
    start: usize,
    /// 不含换行符的行尾偏移。
    end: usize,
    /// 下一行的起始偏移（含本行换行符）。
    next: usize,
    /// 行首空格数；YAML 缩进不允许制表符。
    indent: usize,
    kind: LineKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineKind {
    Blank,
    Comment,
    Content,
}

/// 半开字节区间 `[start, end)`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// 顶层段：从第 0 列的 `key:` 行开始，到下一段或文档结束标记之前；
/// 段尾的空行和第 0 列注释不计入本段，作为下一段的前导内容保留。
#[derive(Clone, Debug)]
pub(crate) struct Section {
    pub(crate) key: String,
    /// 整段区间（含段头行）。
    pub(crate) span: Span,
    /// 段头之后的正文起点。
    pub(crate) body_start: usize,
    /// 段头 `key:` 后同一行还有值（flow 容器、块标量指示符或标量）。
    pub(crate) inline: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DocumentLayout {
    pub(crate) sections: Vec<Section>,
    /// 新增段的插入位置：文档结束标记（`...`/`---`）之前或文本末尾。
    pub(crate) tail: usize,
}

fn lines(text: &str, from: usize, to: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = from;
    while start < to {
        let next = text[start..to]
            .find('\n')
            .map_or(to, |offset| start + offset + 1);
        let mut end = next;
        if text[start..end].ends_with('\n') {
            end -= 1;
        }
        if text[start..end].ends_with('\r') {
            end -= 1;
        }
        let raw = &text[start..end];
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let rest = raw[indent..].trim_start_matches([' ', '\t']);
        let kind = if rest.is_empty() {
            LineKind::Blank
        } else if rest.starts_with('#') {
            LineKind::Comment
        } else {
            LineKind::Content
        };
        lines.push(Line {
            start,
            end,
            next,
            indent,
            kind,
        });
        start = next;
    }
    lines
}

fn content<'a>(text: &'a str, line: &Line) -> &'a str {
    &text[line.start + line.indent..line.end]
}

/// 识别文档标记行：`Some(true)` 表示只有标记（可带注释），`Some(false)` 表示标记后还跟着
/// 内容（如 `--- value`），这种布局不支持；不是标记行时返回 `None`。
fn document_marker(content: &str) -> Option<bool> {
    let rest = content
        .strip_prefix("---")
        .or_else(|| content.strip_prefix("..."))?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    Some(rest.is_empty() || rest.starts_with('#'))
}

fn sequence_entry(content: &str) -> bool {
    content == "-" || content.starts_with("- ") || content.starts_with("-\t")
}

/// 解析 `key:` 行，只接受由字母、数字、`_`、`-` 组成的普通键。
/// 返回键名以及冒号后同一行是否还有值（注释不算值）。
fn plain_key(content: &str) -> Option<(&str, bool)> {
    let length = content
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        .count();
    if length == 0 || content.starts_with('-') {
        return None;
    }
    let rest = content[length..].strip_prefix(':')?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let value = rest.trim_start_matches([' ', '\t']);
    Some((
        &content[..length],
        !(value.is_empty() || value.starts_with('#')),
    ))
}

/// 从 `boundary` 往回跳过空行以及缩进不超过 `indent` 的注释行，返回切片的结束位置。
fn trim_trailing(lines: &[Line], first: usize, boundary: usize, indent: usize) -> usize {
    let mut end = boundary;
    for line in lines[first..]
        .iter()
        .rev()
        .skip_while(|line| line.start >= boundary)
    {
        match line.kind {
            LineKind::Blank => end = line.start,
            LineKind::Comment if line.indent <= indent => end = line.start,
            _ => break,
        }
    }
    end
}

/// 扫描顶层段。根节点不是块式映射（例如整份 flow 文档）或出现无法识别的第 0 列内容时返回 `None`。
pub(crate) fn scan_document(text: &str) -> Option<DocumentLayout> {
    let lines = lines(text, 0, text.len());
    let mut starts: Vec<(usize, String, bool)> = Vec::new();
    let mut tail = text.len();
    let mut seen_content = false;
    for (index, line) in lines.iter().enumerate() {
        if line.kind != LineKind::Content {
            continue;
        }
        let content = content(text, line);
        if line.indent > 0 {
            // 首段之前不允许缩进内容；段内缩进行属于当前段。
            if starts.is_empty() {
                return None;
            }
            continue;
        }
        match document_marker(content) {
            Some(true) if !seen_content => {
                seen_content = true;
                continue;
            }
            Some(true) => {
                tail = line.start;
                break;
            }
            Some(false) => return None,
            None => {}
        }
        if content.starts_with('%') && starts.is_empty() {
            continue;
        }
        seen_content = true;
        if sequence_entry(content) {
            // 顶格列表项属于当前段（不额外缩进的列表）。
            if starts.is_empty() {
                return None;
            }
            continue;
        }
        let (key, inline) = plain_key(content)?;
        starts.push((index, key.to_owned(), inline));
    }
    let mut sections = Vec::with_capacity(starts.len());
    for (position, (index, key, inline)) in starts.iter().enumerate() {
        let boundary = starts
            .get(position + 1)
            .map_or(tail, |(next, _, _)| lines[*next].start);
        let header = lines[*index];
        let end = trim_trailing(&lines, *index + 1, boundary, 0).max(header.next.min(boundary));
        sections.push(Section {
            key: key.clone(),
            span: Span {
                start: header.start,
                end,
            },
            body_start: header.next.min(end),
            inline: *inline,
        });
    }
    Some(DocumentLayout { sections, tail })
}

/// 扫描段内列表项，返回项缩进和每一项的区间。项尾的空行和同缩进（或更浅）注释归入下一项。
pub(crate) fn scan_items(text: &str, section: &Section) -> Option<(usize, Vec<Span>)> {
    if section.inline {
        return None;
    }
    let lines = lines(text, section.body_start, section.span.end);
    let first = lines.iter().find(|line| line.kind == LineKind::Content)?;
    if !sequence_entry(content(text, first)) {
        return None;
    }
    let indent = first.indent;
    let mut starts = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.kind != LineKind::Content || line.indent > indent {
            continue;
        }
        if line.indent < indent || !sequence_entry(content(text, line)) {
            return None;
        }
        starts.push(index);
    }
    let spans = starts
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let boundary = starts
                .get(position + 1)
                .map_or(section.span.end, |next| lines[*next].start);
            Span {
                start: lines[*index].start,
                end: trim_trailing(&lines, *index + 1, boundary, indent)
                    .max(lines[*index].next.min(boundary)),
            }
        })
        .collect();
    Some((indent, spans))
}

/// 扫描单例段内的子键，返回子缩进和每个子键的区间。与子键同缩进的 `- ` 行视为上一子键的
/// 不缩进列表值。
pub(crate) fn scan_keys(text: &str, section: &Section) -> Option<(usize, Vec<(String, Span)>)> {
    if section.inline {
        return None;
    }
    let lines = lines(text, section.body_start, section.span.end);
    let first = lines.iter().find(|line| line.kind == LineKind::Content)?;
    let indent = first.indent;
    if indent == 0 {
        return None;
    }
    let mut starts: Vec<(usize, String)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.kind != LineKind::Content || line.indent > indent {
            continue;
        }
        if line.indent < indent {
            return None;
        }
        let content = content(text, line);
        if sequence_entry(content) {
            if starts.is_empty() {
                return None;
            }
            continue;
        }
        let (key, _) = plain_key(content)?;
        starts.push((index, key.to_owned()));
    }
    let keys = starts
        .iter()
        .enumerate()
        .map(|(position, (index, key))| {
            let boundary = starts
                .get(position + 1)
                .map_or(section.span.end, |(next, _)| lines[*next].start);
            let span = Span {
                start: lines[*index].start,
                end: trim_trailing(&lines, *index + 1, boundary, indent)
                    .max(lines[*index].next.min(boundary)),
            };
            (key.clone(), span)
        })
        .collect();
    Some((indent, keys))
}

/// 粗粒度检测切片中是否出现锚点（`&`）、别名（`*`）或 merge key（`<<`）。
///
/// 跳过注释、引号字符串（含跨行）和块标量正文；只在节点起始位置识别指示符，普通标量中间的
/// `&`/`*`（例如 URL 查询串）不算。结果只用于拒绝编辑含锚点的条目，误报只会导致拒绝，
/// 不会造成错误写入；漏报由写回后的语义复核兜底。
pub(crate) fn contains_anchor_syntax(text: &str) -> bool {
    let mut quote: Option<u8> = None;
    let mut block_parent: Option<usize> = None;
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let bytes = line.as_bytes();
        let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
        if let Some(parent) = block_parent {
            if line.trim().is_empty() || indent > parent {
                continue;
            }
            block_parent = None;
        }
        let mut index = if quote.is_some() { 0 } else { indent };
        let mut token_start = quote.is_none();
        // 本行最近一个节点（键或列表项）的起始列，用于确定块标量正文的缩进下限。
        let mut node_column = indent;
        while index < bytes.len() {
            let byte = bytes[index];
            if let Some(open) = quote {
                if open == b'\'' && byte == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                        continue;
                    }
                    quote = None;
                } else if open == b'"' && byte == b'\\' {
                    index += 2;
                    continue;
                } else if open == b'"' && byte == b'"' {
                    quote = None;
                }
                index += 1;
                continue;
            }
            let separated = |at: usize| matches!(bytes.get(at), None | Some(b' ' | b'\t' | b'\r'));
            match byte {
                b' ' | b'\t' => {}
                b'#' if index == 0 || matches!(bytes[index - 1], b' ' | b'\t') => break,
                b'[' | b'{' | b',' | b']' | b'}' => token_start = true,
                b'-' | b'?' if token_start && separated(index + 1) => node_column = index,
                b':' if separated(index + 1) => token_start = true,
                b'&' | b'*' if token_start => return true,
                b'<' if token_start && line[index..].starts_with("<<") => {
                    let rest = line[index + 2..].trim_start_matches([' ', '\t']);
                    if rest.starts_with(':') {
                        return true;
                    }
                    token_start = false;
                }
                b'|' | b'>' if token_start => {
                    block_parent = Some(node_column);
                    break;
                }
                b'\'' | b'"' if token_start => {
                    quote = Some(byte);
                    token_start = false;
                }
                _ => {
                    if token_start {
                        node_column = index;
                    }
                    token_start = false;
                }
            }
            index += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests;
