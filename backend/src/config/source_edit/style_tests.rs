use super::*;
use yaml_serde::Value;

fn tree(source: &str) -> Value {
    yaml_serde::from_str(source).unwrap()
}

#[test]
fn appended_resources_and_nested_fields_use_block_style() {
    for newline in ["\n", "\r\n"] {
        for indent in [2, 4] {
            let pad = " ".repeat(indent);
            let source = format!(
                "# 保留头部\nrule_set:\n{pad}- name: local # 保留注释\n{pad}  type: file\n{pad}  path: ./rules/local.dat\n"
            ).replace('\n', newline);
            let original = tree(&source);
            let mut intended = original.clone();
            intended["rule_set"].as_sequence_mut().unwrap().push(tree(
                "name: geosite\ntype: remote\nformat: dat\nurl: https://rules.example/geosite.dat\n",
            ));
            intended["rule_set"][0]["options"] = tree("enabled: true\nips: [192.0.2.1, '::']");
            let result = edit_document(&source, &original, &intended).unwrap();
            assert_eq!(tree(&result), intended);
            assert!(result.starts_with(&format!("# 保留头部{newline}")));
            assert!(result.contains("name: local # 保留注释"));
            assert!(
                result.contains(&format!("{pad}- name: geosite{newline}{pad}  type: remote")),
                "{result}"
            );
            assert!(
                result.contains(&format!(
                    "{pad}  options:{newline}{pad}  {}enabled: true",
                    " ".repeat(indent)
                )),
                "{result}"
            );
            assert!(!result.contains('{'), "{result}");
            assert!(!result.contains('['), "{result}");
            if newline == "\r\n" {
                assert!(!result.replace("\r\n", "").contains('\n'));
            }
            let mut next = intended.clone();
            next["rule_set"][1]["proxy"] = Value::String("proxy".into());
            let repeated = edit_document(&result, &intended, &next).unwrap();
            assert_eq!(tree(&repeated), next);
            assert!(!repeated.contains('{'), "{repeated}");
        }
    }
}

#[test]
fn empty_collections_expand_as_blocks_without_losing_inline_comments() {
    for newline in ["\n", "\r\n"] {
        for ending in ["", "\n", "\n...\n"] {
            let source =
                format!("clients: [] # 客户端\ndns: {{}} # DNS{ending}").replace('\n', newline);
            let original = tree(&source);
            let mut intended = original.clone();
            intended["clients"] = tree("[{name: phone, match: {ips: []}}]");
            intended["dns"] = tree("cache: {enabled: true}");
            let result = edit_document(&source, &original, &intended).unwrap();
            assert_eq!(tree(&result), intended);
            assert!(result.contains(&format!("clients: # 客户端{newline}  - name: phone{newline}    match:{newline}      ips: []")), "{result}");
            assert!(
                result.contains(&format!(
                    "dns: # DNS{newline}  cache:{newline}    enabled: true"
                )),
                "{result}"
            );
            assert_eq!(
                edit_document(&result, &intended, &intended).unwrap(),
                result
            );
            if ending.contains("...") {
                assert!(result.ends_with(&format!("...{newline}")));
            }
        }
    }
}

#[test]
fn shortened_block_sequences_and_replaced_values_remain_blocks() {
    let source = "strategy:\n  rules:\n    - name: first\n      ips:\n        - 192.0.2.1\n        - 192.0.2.2\n    - name: second\n  extra: old # 保留字段注释\nlogs: {enable: false}\n";
    let original = tree(source);
    let mut intended = original.clone();
    intended["strategy"]["rules"]
        .as_sequence_mut()
        .unwrap()
        .pop();
    intended["strategy"]["extra"] = tree("nested: [a, b]");
    let result = edit_document(source, &original, &intended).unwrap();
    assert_eq!(tree(&result), intended);
    assert!(
        result.contains("rules:\n    - name: first\n      ips:\n        - 192.0.2.1"),
        "{result}"
    );
    assert!(
        result.contains("extra: # 保留字段注释\n    nested:\n      - a\n      - b"),
        "{result}"
    );
    assert!(result.ends_with("logs: {enable: false}\n"));
}

#[test]
fn nonempty_flow_containers_and_their_empty_children_stay_inline() {
    let source = "clients: [{name: desktop, match: {ips: []}}] # 保留 flow\nlogs: {enable: false, level: info}\n";
    let original = tree(source);
    let mut intended = original.clone();
    intended["clients"][0]["match"]["ips"] = tree("[192.0.2.1]");
    intended["clients"]
        .as_sequence_mut()
        .unwrap()
        .push(tree("{name: phone}"));
    intended["logs"]["path"] = Value::String("./logs/test.log".into());
    let result = edit_document(source, &original, &intended).unwrap();
    assert_eq!(tree(&result), intended);
    assert_eq!(result.lines().count(), 2);
    assert!(result.starts_with("clients: ["));
    assert!(result.contains("# 保留 flow"));
}

#[test]
fn new_block_fragments_escape_strings_and_preserve_multiline_values() {
    let source = "logs:\n  enable: false\n";
    let original = tree(source);
    for text in [
        "true",
        "null",
        "123",
        "a: b # c",
        "'quoted'",
        "第一行\n第二行\n\n",
        "\n",
        "\n\n",
        "\r\n\t\\\"",
        "",
        "---",
        "...",
    ] {
        let mut intended = original.clone();
        intended["logs"]["extra"] = Value::Mapping(
            [(
                Value::String("special: key".into()),
                Value::String(text.into()),
            )]
            .into_iter()
            .collect(),
        );
        let result = edit_document(source, &original, &intended).unwrap();
        assert_eq!(tree(&result), intended, "{result}");
        assert!(result.contains("  extra:\n"), "{result}");
    }
}

#[test]
fn emptied_block_containers_can_be_filled_again() {
    let source = "clients:\n  - name: desktop\nlogs:\n  enable: false\n";
    let original = tree(source);
    let mut empty = original.clone();
    empty["clients"] = tree("[]");
    empty["logs"] = tree("{}");
    let result = edit_document(source, &original, &empty).unwrap();
    assert_eq!(tree(&result), empty);
    let filled = edit_document(&result, &empty, &original).unwrap();
    assert_eq!(tree(&filled), original);
    assert!(filled.contains("clients:\n  - name: desktop\n"), "{filled}");
    assert!(filled.contains("logs:\n  enable: false\n"), "{filled}");
}

#[test]
fn append_to_indentless_sequence_keeps_existing_items_and_block_layout() {
    let source = "rule_set:\n- name: local # 原列表顶格\n  type: file\n  path: ./local.dat\nlogs:\n  enable: false\n";
    let original = tree(source);
    let mut intended = original.clone();
    intended["rule_set"]
        .as_sequence_mut()
        .unwrap()
        .push(tree("name: geosite\ntype: remote"));
    let result = edit_document(source, &original, &intended).unwrap();
    assert_eq!(tree(&result), intended);
    assert!(result.starts_with(source.split("logs:").next().unwrap()));
    assert!(
        result.contains("\n- name: geosite\n  type: remote\nlogs:\n"),
        "{result}"
    );
}

#[test]
fn expanded_empty_flow_containers_preserve_internal_comments() {
    for newline in ["\n", "\r\n"] {
        let source = "clients: [ # 客户端说明含 ] 和 ,\n  # 第二条客户端说明\n] # 客户端尾注释\ndns: { # DNS 说明含 }\n  # 第二条 DNS 说明\n} # DNS 尾注释\n".replace('\n', newline);
        let original = tree(&source);
        let mut intended = original.clone();
        intended["clients"] = tree("[{name: phone}]");
        intended["dns"] = tree("cache: {enabled: true}");
        let result = edit_document(&source, &original, &intended).unwrap();
        assert_eq!(tree(&result), intended);
        let expected = "clients: # 客户端尾注释\n  # 客户端说明含 ] 和 ,\n  # 第二条客户端说明\n  - name: phone\ndns: # DNS 尾注释\n  # DNS 说明含 }\n  # 第二条 DNS 说明\n  cache:\n    enabled: true\n".replace('\n', newline);
        assert_eq!(result, expected);
        let mut next = intended.clone();
        next["clients"][0]["name"] = Value::String("mobile".into());
        let repeated = edit_document(&result, &intended, &next).unwrap();
        assert_eq!(tree(&repeated), next);
        for comment in [
            "# 客户端说明含 ] 和 ,",
            "# 第二条客户端说明",
            "# DNS 说明含 }",
            "# 第二条 DNS 说明",
        ] {
            assert!(repeated.contains(comment), "{repeated}");
        }
    }
}

#[test]
fn flow_appends_handle_trailing_commas_and_comments_repeatedly() {
    for newline in ["\n", "\r\n"] {
        for source in [
            "logs: {enable: false,}\nclients: [{name: desktop},]\n",
            "logs: {enable: false, # 保留日志说明\n}\nclients: [{name: desktop}, # 保留客户端说明\n]\n",
            "logs: {enable: false # 注释中的逗号 ,\n}\nclients: [{name: desktop} # 注释中的逗号 ,\n]\n",
            "logs: {path: 'ends,',}\nclients: ['ends,',]\n",
            "logs: {path: 'ends,'}\nclients: ['ends,']\n",
        ] {
            let source = source.replace('\n', newline);
            let original = tree(&source);
            let mut intended = original.clone();
            intended["logs"]["level"] = Value::String("info".into());
            intended["clients"]
                .as_sequence_mut()
                .unwrap()
                .push(tree("{name: phone}"));
            let result = edit_document(&source, &original, &intended).unwrap();
            assert_eq!(tree(&result), intended);
            assert!(result.starts_with("logs: {"));
            assert!(result.contains("clients: ["));
            let mut next = intended.clone();
            next["logs"]["extra"] = Value::String("next".into());
            next["clients"]
                .as_sequence_mut()
                .unwrap()
                .push(tree("{name: tablet}"));
            let repeated = edit_document(&result, &intended, &next).unwrap();
            assert_eq!(tree(&repeated), next);
            for comment in source
                .lines()
                .filter_map(|line| line.find('#').map(|index| &line[index..]))
            {
                assert!(repeated.contains(comment), "{repeated}");
            }
            if newline == "\r\n" {
                assert!(!repeated.replace("\r\n", "").contains('\n'));
            }
        }
    }
}
