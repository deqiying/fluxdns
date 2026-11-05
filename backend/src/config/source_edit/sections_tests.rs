use super::*;
use yaml_serde::Value;

const PREFIX: &str = "# 保留原始注释与代理表达\noutbound:\n- name: proxy\n  type: socks5\n  proxy_url: {env: TEST_PROXY}\n\n";

fn tree(source: &str) -> Value {
    yaml_serde::from_str(source).unwrap()
}

#[test]
fn mixed_block_layouts_preserve_quoted_keys_comments_and_scalar_contents() {
    for newline in ["\n", "\r\n"] {
        for ending in ["", "\n", "\n...\n"] {
            let source = format!(
                "---\n{PREFIX}hosts: |\n  clients: not a top-level key\n  # literal comment and {{braces}}\n\n'clients':\n  - name: desktop # 保留已有客户端\n    client_id: Desktop-01{ending}"
            ).replace('\n', newline);
            let original = tree(&source);
            let mut intended = original.clone();
            intended["clients"]
                .as_sequence_mut()
                .unwrap()
                .push(tree("{name: phone, client_id: phone-01, match: {ips: []}}"));
            let result = edit_document(&source, &original, &intended).unwrap();
            assert_eq!(tree(&result), intended);
            assert!(result.starts_with(source.split("'clients':").next().unwrap()));
            assert!(result.contains(&format!("name: desktop # 保留已有客户端{newline}")));
            if newline == "\r\n" {
                assert!(!result.replace("\r\n", "").contains('\n'));
            }
            if ending.contains("...") {
                assert!(result.ends_with(&format!("...{newline}")));
            }
        }
    }
}

#[test]
fn simultaneous_edits_and_new_modules_stay_before_document_end() {
    for ending in ["", "\n...\n"] {
        let source = format!("{PREFIX}clients: []{ending}");
        let original = tree(&source);
        let mut intended = original.clone();
        intended["outbound"][0]["name"] = Value::String("renamed".into());
        intended["clients"] = tree("[{name: phone, client_id: phone-01}]");
        intended["logs"] = tree("{enable: false, level: info, path: ./logs/test.log}");
        let result = edit_document(&source, &original, &intended).unwrap();
        assert_eq!(tree(&result), intended);
        assert!(result.contains("outbound:\n- name:"));
        assert!(result.contains("proxy_url: {env: TEST_PROXY}"));
        if !ending.is_empty() {
            assert!(result.ends_with(ending));
        }
        // 新增的行内模块和已有顶格列表仍可在下一次编辑中定位。
        let mut next = intended.clone();
        next["logs"]["enable"] = Value::Bool(true);
        let repeated = edit_document(&result, &intended, &next).unwrap();
        assert_eq!(tree(&repeated), next);
    }
}

#[test]
fn multiline_quoted_strings_are_not_split_at_apparent_root_keys() {
    let source = format!(
        "{PREFIX}description: \"first line\nclients: text inside quoted scalar\nlast line\"\nclients: []\n"
    );
    let original = tree(&source);
    let mut intended = original.clone();
    intended["clients"] = tree("[{name: phone, client_id: phone-01}]");
    let result = edit_document(&source, &original, &intended).unwrap();
    assert_eq!(tree(&result), intended);
    assert!(
        result
            .contains("description: \"first line\nclients: text inside quoted scalar\nlast line\"")
    );
}

#[test]
fn section_compatibility_rejects_ambiguous_or_mismatched_sources() {
    let source = format!("{PREFIX}clients: []\n");
    let original = tree(&source);
    let mut intended = original.clone();
    intended["clients"] = tree("[{name: phone, client_id: phone-01}]");
    let mut inconsistent = original.clone();
    inconsistent["outbound"][0]["name"] = Value::String("different".into());
    assert_eq!(
        sections::edit_block_sections(&source, &inconsistent, &intended),
        Err(SourceEditError::UnsupportedSyntax),
    );
    for invalid in [
        format!("{source}clients: []\n"),
        format!("{source}---\nclients: []\n"),
    ] {
        assert!(sections::edit_block_sections(&invalid, &original, &intended).is_err());
    }
    for anchored in [
        source.replace("name: proxy", "name: &proxy proxy"),
        source.replace("clients: []", "clients: &clients []\nother: *clients"),
    ] {
        let old = tree(&anchored);
        let mut new = old.clone();
        new["clients"] = intended["clients"].clone();
        assert_eq!(
            edit_document(&anchored, &old, &new),
            Err(SourceEditError::UnsupportedSyntax)
        );
    }
}
