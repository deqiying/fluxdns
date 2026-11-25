use super::*;

fn slice(text: &str, span: Span) -> &str {
    &text[span.start..span.end]
}

fn section<'a>(layout: &'a DocumentLayout, key: &str) -> &'a Section {
    layout
        .sections
        .iter()
        .find(|section| section.key == key)
        .unwrap()
}

#[test]
fn indented_and_indentless_lists_split_into_items() {
    for (text, indent) in [
        (
            "listener:\n  - name: a\n    port: 1\n  - name: b\n    port: 2\ndns: {}\n",
            2,
        ),
        (
            "listener:\n- name: a\n  port: 1\n- name: b\n  port: 2\ndns: {}\n",
            0,
        ),
    ] {
        let layout = scan_document(text).unwrap();
        assert_eq!(
            layout
                .sections
                .iter()
                .map(|section| section.key.as_str())
                .collect::<Vec<_>>(),
            ["listener", "dns"]
        );
        let (found, spans) = scan_items(text, section(&layout, "listener")).unwrap();
        assert_eq!(found, indent);
        assert_eq!(spans.len(), 2);
        assert!(slice(text, spans[0]).trim_start().starts_with("- name: a"));
        assert!(slice(text, spans[1]).ends_with("port: 2\n"));
        assert!(section(&layout, "dns").inline);
        assert!(scan_items(text, section(&layout, "dns")).is_none());
    }
}

#[test]
fn block_scalars_and_multi_line_quotes_are_not_item_or_key_starts() {
    let text = "hosts:\n  - name: a\n    hosts: |\n      - not an item\n      key: not a key\n  - name: b\n    hosts: \"first\n      - still quoted\"\n";
    let layout = scan_document(text).unwrap();
    let (_, spans) = scan_items(text, section(&layout, "hosts")).unwrap();
    assert_eq!(spans.len(), 2);
    assert!(slice(text, spans[0]).contains("key: not a key"));
    assert!(slice(text, spans[1]).contains("still quoted"));
}

#[test]
fn trailing_blank_lines_and_comments_belong_to_the_next_entry() {
    let text = "strategy:\n  # 段首注释\n  - name: a\n    rules: []\n    # 项内更深缩进的注释\n\n  # 下一项的注释\n  - name: b\n    rules: []\n\n# 下一段的注释\nlogs:\n  enable: false\n";
    let layout = scan_document(text).unwrap();
    let strategy = section(&layout, "strategy");
    assert!(slice(text, strategy.span).ends_with("  - name: b\n    rules: []\n"));
    let (_, spans) = scan_items(text, strategy).unwrap();
    assert_eq!(
        slice(text, spans[0]),
        "  - name: a\n    rules: []\n    # 项内更深缩进的注释\n"
    );
    assert_eq!(slice(text, spans[1]), "  - name: b\n    rules: []\n");
    let logs = section(&layout, "logs");
    let (indent, keys) = scan_keys(text, logs).unwrap();
    assert_eq!(indent, 2);
    assert_eq!(keys[0].0, "enable");
}

#[test]
fn crlf_and_missing_final_newline_are_preserved_in_spans() {
    let text =
        "logs:\r\n  enable: false\r\n  level: info\r\nclients:\r\n  - name: a\r\n    client_id: A";
    let layout = scan_document(text).unwrap();
    let (_, keys) = scan_keys(text, section(&layout, "logs")).unwrap();
    assert_eq!(slice(text, keys[1].1), "  level: info\r\n");
    let (_, spans) = scan_items(text, section(&layout, "clients")).unwrap();
    assert_eq!(slice(text, spans[0]), "  - name: a\r\n    client_id: A");
    assert_eq!(layout.tail, text.len());
}

#[test]
fn document_markers_bound_the_sections() {
    let text = "%YAML 1.2\n---\nversion: 2\nlogs:\n  enable: false\n...\n";
    let layout = scan_document(text).unwrap();
    assert_eq!(layout.sections.len(), 2);
    assert_eq!(
        slice(text, section(&layout, "logs").span),
        "logs:\n  enable: false\n"
    );
    assert_eq!(&text[layout.tail..], "...\n");
    assert!(scan_document("--- {version: 2}\n").is_none());
}

#[test]
fn flow_documents_and_flow_sections_cannot_be_split() {
    assert!(scan_document("{version: 2, logs: {enable: false}}\n").is_none());
    let text = "listener: [{name: a}, {name: b}]\nlogs: {enable: false}\n";
    let layout = scan_document(text).unwrap();
    assert!(scan_items(text, section(&layout, "listener")).is_none());
    assert!(scan_keys(text, section(&layout, "logs")).is_none());
}

#[test]
fn indentless_sequence_under_sub_key_stays_with_that_key() {
    let text = "dns:\n  servers:\n  - a\n  - b\n  cache: {}\n";
    let layout = scan_document(text).unwrap();
    let (_, keys) = scan_keys(text, section(&layout, "dns")).unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(slice(text, keys[0].1), "  servers:\n  - a\n  - b\n");
}

#[test]
fn anchor_detection_skips_strings_comments_and_block_scalars() {
    for text in [
        "a: &x 1\n",
        "a: *x\n",
        "- <<: *base\n",
        "a: [1, &y 2]\n",
        "- &item\n  name: a\n",
    ] {
        assert!(contains_anchor_syntax(text), "{text}");
    }
    for text in [
        "url: https://example.com/?a=1&b=2\n",
        "hosts: \"&not anchor\"\n",
        "hosts: '*.example.com'\n",
        "a: 1 # &comment\n",
        "rule: |\n  *.example.com\n  &literal\nnext: 1\n",
        "- name: a\n  hosts: >-\n    *folded\n  port: 1\n",
        "text: \"line one\n  &still quoted\"\n",
    ] {
        assert!(!contains_anchor_syntax(text), "{text}");
    }
    // 块标量结束后的同级键仍会被检查。
    assert!(contains_anchor_syntax(
        "rule: |\n  *.example.com\nnext: *x\n"
    ));
}
