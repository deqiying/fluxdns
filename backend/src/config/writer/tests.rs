use std::path::Path;

use super::*;
use crate::config::change::{ConfigChange, EditError, ResourceMutation, apply_changes};
use crate::config::contract::{DnsV2, ResolveLogV2};
use crate::config::model::{EcsDto, EcsMode, StrategyDto, StrategyRuleDto, TtlOverrideDto};

/// 截图场景：策略 `default` 含 4 条有序规则，带规则级和策略级 ECS 覆盖；`lan` 带项内注释。
const SCENARIO: &str = r#"# 截图场景夹具
version: 2
work:
  path: .
  rules_path: ./rules
database:
  type: sqlite
  path: ./data/statistics.sqlite3
  records_path: ./data/queries
logs:
  enable: false
  level: info
  path: ./logs/fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 18080
dns:
  # 全局 ECS（子键注释）
  edns_client_subnet:
    mode: client
  resolve_log:
    enable: true
listener:
  - name: local
    type: udp
    addresses: [127.0.0.1]
    port: 15353
    strategy: default
upstreams:
  - name: inner-group
    type: hosts
    format: hosts
    hosts: "127.0.0.1 inner"
  - name: outside-group
    type: hosts
    format: hosts
    hosts: "127.0.0.2 outside"
strategy:
  # 默认策略（项外注释，保留）
  - name: default
    rules:
      - hosts: host-dns
      - rule_set: cn-site
        upstream: inner-group
        edns_client_subnet:
          mode: custom
          custom_ip: 120.85.32.0/24
      - rule_set: proxy-sites
        upstream: outside-group
      - hosts: host-lan
    edns_client_subnet:
      mode: client
    default_upstream: outside-group

  # 未修改的项逐字节保留，包括项内注释
  - name: lan
    rules:
      - hosts: host-dns   # 局域网主机
    default_upstream: inner-group
hosts:
  - name: host-dns
    type: const
    format: hosts
    hosts: "127.0.0.1 a.local"
  - name: host-lan
    type: const
    format: hosts
    hosts: "127.0.0.1 b.local"
rule_set:
  - name: cn-site
    type: const
    format: json
    rule: '{"domain": ["cn.example"]}'
  - name: proxy-sites
    type: const
    format: json
    rule: '{"domain": ["proxy.example"]}'
"#;

const DEFAULT_START: &str = "  - name: default\n";
const DEFAULT_END: &str = "\n  # 未修改的项逐字节保留";

fn render_changes(source: &str, changes: &[ConfigChange]) -> Result<String, EditError> {
    let base = ConfigV2::parse(source.as_bytes()).expect("夹具必须合法");
    let applied = apply_changes(&base, changes)?;
    let text = render(source, &base, &applied.config, &applied.renames)?;
    // 写回结果必须能作为新的活动源再次解析，并与目标配置语义一致。
    let parsed = ConfigV2::parse(text.as_bytes()).expect("写回结果必须可解析");
    parsed
        .resolve_paths(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../_fluxdns/p1-writer/config.yaml"),
        )
        .expect("写回结果路径校验");
    assert_eq!(
        source_model(&parsed).unwrap(),
        source_model(&applied.config).unwrap()
    );
    Ok(text)
}

fn edit_default(mutate: impl FnOnce(&mut StrategyDto)) -> Result<String, EditError> {
    let base = ConfigV2::parse(SCENARIO.as_bytes()).unwrap();
    let mut strategy = base.strategy[0].clone();
    mutate(&mut strategy);
    render_changes(
        SCENARIO,
        &[ConfigChange::Strategy(ResourceMutation::Update {
            original_name: "default".into(),
            value: strategy,
        })],
    )
}

/// 断言 `default` 条目之外（含项外注释、`lan` 及其项内注释、其他模块）逐字节不变。
fn assert_only_default_changed(output: &str) {
    let prefix_end = SCENARIO.find(DEFAULT_START).unwrap();
    let suffix_start = SCENARIO.find(DEFAULT_END).unwrap();
    assert!(
        output.starts_with(&SCENARIO[..prefix_end]),
        "前缀被改动：\n{output}"
    );
    assert!(
        output.ends_with(&SCENARIO[suffix_start..]),
        "后缀被改动：\n{output}"
    );
}

fn rule(hosts: Option<&str>, rule_set: Option<&str>, upstream: Option<&str>) -> StrategyRuleDto {
    StrategyRuleDto {
        rule_set: rule_set.map(Into::into),
        hosts: hosts.map(Into::into),
        upstream: upstream.map(Into::into),
        edns_client_subnet: None,
    }
}

#[test]
fn swapping_rules_rewrites_only_the_edited_strategy() {
    // 基线代码上这一场景返回 CANDIDATE_SOURCE_UNSUPPORTED（列表项首键需要删除）。
    let output = edit_default(|strategy| strategy.rules.swap(0, 1)).unwrap();
    assert_only_default_changed(&output);
    let parsed = ConfigV2::parse(output.as_bytes()).unwrap();
    assert_eq!(
        parsed.strategy[0].rules[0].rule_set.as_deref(),
        Some("cn-site")
    );
    assert_eq!(
        parsed.strategy[0].rules[1].hosts.as_deref(),
        Some("host-dns")
    );
    assert!(output.contains(
        "  - name: default\n    rules:\n      - rule_set: cn-site\n        upstream: inner-group\n        edns_client_subnet:\n          mode: custom\n          custom_ip: 120.85.32.0/24\n      - hosts: host-dns\n"
    ));
}

#[test]
fn moving_switching_adding_and_removing_rules_and_ecs_all_save() {
    type Mutation = Box<dyn Fn(&mut StrategyDto)>;
    let cases: Vec<Mutation> = vec![
        // 上移 / 下移
        Box::new(|strategy| strategy.rules.swap(2, 3)),
        Box::new(|strategy| {
            let rule = strategy.rules.remove(0);
            strategy.rules.push(rule);
        }),
        // 类型切换：hosts -> rule_set + upstream，rule_set -> hosts
        Box::new(|strategy| {
            strategy.rules[3] = rule(None, Some("cn-site"), Some("inner-group"));
            strategy.rules[2] = rule(Some("host-lan"), None, None);
        }),
        // 新增、删除规则
        Box::new(|strategy| {
            strategy
                .rules
                .push(rule(None, Some("proxy-sites"), Some("inner-group")))
        }),
        Box::new(|strategy| {
            strategy.rules.remove(1);
        }),
        // 规则级 ECS 新增与删除、策略级 ECS 删除
        Box::new(|strategy| {
            strategy.rules[2].edns_client_subnet = Some(EcsDto {
                mode: EcsMode::Custom,
                custom_ip: Some("120.85.11.0/24".parse().unwrap()),
            });
            strategy.rules[1].edns_client_subnet = None;
            strategy.edns_client_subnet = None;
        }),
    ];
    for (index, case) in cases.iter().enumerate() {
        let output = edit_default(case).unwrap_or_else(|error| panic!("场景 {index}: {error}"));
        assert_only_default_changed(&output);
    }
}

#[test]
fn unchanged_resubmission_keeps_the_source_untouched() {
    let output = edit_default(|_| {}).unwrap();
    assert_eq!(output, SCENARIO);
}

#[test]
fn rename_rewrites_only_referencing_entries() {
    let base = ConfigV2::parse(SCENARIO.as_bytes()).unwrap();
    let mut hosts = base.hosts[1].clone();
    if let crate::config::model::HostsResourceDto::Const { name, .. } = &mut hosts {
        *name = "host-lan-2".into();
    }
    let output = render_changes(
        SCENARIO,
        &[ConfigChange::Hosts(ResourceMutation::Update {
            original_name: "host-lan".into(),
            value: hosts,
        })],
    )
    .unwrap();
    // 只有引用 host-lan 的 default 策略和被改名的 hosts 条目变化；lan 策略及项内注释、
    // 未改名的 host-dns 条目和其他模块逐字节保留。
    let prefix_end = SCENARIO.find(DEFAULT_START).unwrap();
    assert!(output.starts_with(&SCENARIO[..prefix_end]));
    let lan_to_host_lan = &SCENARIO
        [SCENARIO.find(DEFAULT_END).unwrap()..SCENARIO.find("  - name: host-lan\n").unwrap()];
    assert!(output.contains(lan_to_host_lan), "{output}");
    assert!(output.ends_with(&SCENARIO[SCENARIO.find("rule_set:\n").unwrap()..]));
    assert!(output.contains("      - hosts: host-lan-2\n"));
    assert!(output.contains("  - name: host-lan-2\n    type: const\n"));
}

#[test]
fn creating_entries_in_missing_or_empty_sections_uses_block_style() {
    let create = || {
        vec![ConfigChange::Outbound(ResourceMutation::Create {
            value: serde_json::from_str(
                r#"{"name":"proxy","type":"socks5","proxy_url":{"env":"PROXY_URL"}}"#,
            )
            .unwrap(),
        })]
    };
    // 段不存在：追加到文档末尾。
    let output = render_changes(SCENARIO, &create()).unwrap();
    assert!(output.starts_with(SCENARIO));
    assert!(output.ends_with(
        "outbound:\n  - name: proxy\n    type: socks5\n    proxy_url:\n      env: PROXY_URL\n"
    ));
    // 段为空的 flow 写法：整段写成块式，后续模块不受影响。
    let empty = SCENARIO.replace(
        "hosts:\n  - name: host-dns",
        "outbound: [] # 首次新增\nhosts:\n  - name: host-dns",
    );
    let output = render_changes(&empty, &create()).unwrap();
    assert!(output.contains("outbound:\n  - name: proxy\n    type: socks5\n    proxy_url:\n      env: PROXY_URL\nhosts:\n  - name: host-dns"));
    // 文件末尾没有换行时追加内容不能接在最后一行后面。
    let trimmed = SCENARIO.trim_end_matches('\n');
    let output = render_changes(trimmed, &create()).unwrap();
    assert!(output.contains("'{\"domain\": [\"proxy.example\"]}'\noutbound:\n"));
}

#[test]
fn singleton_sub_keys_are_added_replaced_and_removed_in_place() {
    let base = ConfigV2::parse(SCENARIO.as_bytes()).unwrap();
    let mut dns: DnsV2 = base.dns.clone();
    dns.edns_client_subnet = None;
    dns.resolve_log = Some(ResolveLogV2 { enable: false });
    dns.ttl_override = Some(TtlOverrideDto {
        enabled: Some(true),
        min: Some(std::time::Duration::from_secs(60)),
        max: None,
    });
    let output = render_changes(SCENARIO, &[ConfigChange::Dns(dns)]).unwrap();
    // edns_client_subnet 子键（含其前导注释之后的正文）被删除，resolve_log 原位替换，ttl_override 追加。
    assert!(
        output.contains("dns:\n  # 全局 ECS（子键注释）\n  resolve_log:\n    enable: false\n  ttl_override:\n    enabled: true\n    min: 1m\nlistener:\n"),
        "{output}"
    );
    let before_dns = SCENARIO.find("dns:\n").unwrap();
    let after_dns = SCENARIO.find("listener:\n").unwrap();
    assert!(output.starts_with(&SCENARIO[..before_dns]));
    assert!(output.ends_with(&SCENARIO[after_dns..]));

    let mut logs = base.logs.clone();
    logs.level = crate::config::model::LogLevelDto::Debug;
    let output = render_changes(SCENARIO, &[ConfigChange::Logs(logs)]).unwrap();
    assert_eq!(
        output,
        SCENARIO.replace("  level: info\n", "  level: debug\n")
    );
}

#[test]
fn entries_with_anchors_or_aliases_are_rejected() {
    let anchored = SCENARIO
        .replace(
            "    hosts: \"127.0.0.1 inner\"",
            "    hosts: &shared \"127.0.0.1 inner\"",
        )
        .replace("    hosts: \"127.0.0.2 outside\"", "    hosts: *shared");
    let base = ConfigV2::parse(anchored.as_bytes()).unwrap();
    for index in [0, 1] {
        let mut upstream = base.upstreams[index].clone();
        if let crate::config::model::UpstreamDto::Hosts { hosts, .. } = &mut upstream {
            *hosts = "127.0.0.9 changed".into();
        }
        let name = upstream.name().to_owned();
        let error = render_changes(
            &anchored,
            &[ConfigChange::Upstreams(ResourceMutation::Update {
                original_name: name,
                value: upstream,
            })],
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                EditError::Layout {
                    module: "upstreams",
                    ..
                }
            ),
            "{error:?}"
        );
    }
    // 锚点之外的条目照常写回。
    let output = edit_default(|strategy| strategy.rules.swap(0, 1));
    assert!(output.is_ok());
    let base = ConfigV2::parse(anchored.as_bytes()).unwrap();
    let mut strategy = base.strategy[0].clone();
    strategy.rules.swap(0, 1);
    let output = render_changes(
        &anchored,
        &[ConfigChange::Strategy(ResourceMutation::Update {
            original_name: "default".into(),
            value: strategy,
        })],
    )
    .unwrap();
    assert!(output.contains("hosts: &shared \"127.0.0.1 inner\""));
    assert!(output.contains("hosts: *shared"));
}

#[test]
fn unprovable_list_layout_falls_back_to_whole_section() {
    // 单行 flow 列表无法逐项定位：整段替换，仍须通过语义复核。
    let flow = SCENARIO.replace(
        "listener:\n  - name: local\n    type: udp\n    addresses: [127.0.0.1]\n    port: 15353\n    strategy: default\n",
        "listener: [{name: local, type: udp, addresses: [127.0.0.1], port: 15353, strategy: default}]\n",
    );
    let base = ConfigV2::parse(flow.as_bytes()).unwrap();
    let mut listener: serde_json::Value = serde_json::to_value(&base.listener[0]).unwrap();
    listener["port"] = 15354.into();
    let output = render_changes(
        &flow,
        &[ConfigChange::Listener(ResourceMutation::Update {
            original_name: "local".into(),
            value: serde_json::from_value(listener).unwrap(),
        })],
    )
    .unwrap();
    assert!(
        output.contains(
            "listener:\n  - name: local\n    type: udp\n    addresses:\n      - 127.0.0.1\n"
        ),
        "{output}"
    );
    assert!(output.contains("    port: 15354\n"));
    assert!(output.starts_with(&flow[..flow.find("listener:").unwrap()]));
}

#[test]
fn multi_line_strings_render_as_literal_blocks_and_round_trip() {
    let base = ConfigV2::parse(SCENARIO.as_bytes()).unwrap();
    for text in [
        "127.0.0.1 a.local\n127.0.0.2 c.local\n",
        "127.0.0.1 a.local\n\n127.0.0.2 c.local",
        "  leading space\nsecond\n",
        "trailing newlines\n\n",
        "tab\tinside\nline # not a comment\n",
    ] {
        let mut hosts = base.hosts[0].clone();
        if let crate::config::model::HostsResourceDto::Const { hosts: content, .. } = &mut hosts {
            *content = text.into();
        }
        let output = render_changes(
            SCENARIO,
            &[ConfigChange::Hosts(ResourceMutation::Update {
                original_name: "host-dns".into(),
                value: hosts,
            })],
        )
        .unwrap();
        let parsed = ConfigV2::parse(output.as_bytes()).unwrap();
        let crate::config::model::HostsResourceDto::Const { hosts: content, .. } = &parsed.hosts[0]
        else {
            panic!("const hosts expected");
        };
        assert_eq!(content, text, "{output}");
    }
}

#[test]
fn crlf_sources_keep_crlf_line_endings() {
    let source = SCENARIO.replace('\n', "\r\n");
    let base = ConfigV2::parse(source.as_bytes()).unwrap();
    let mut strategy = base.strategy[0].clone();
    strategy.rules.swap(0, 1);
    let output = render_changes(
        &source,
        &[ConfigChange::Strategy(ResourceMutation::Update {
            original_name: "default".into(),
            value: strategy,
        })],
    )
    .unwrap();
    assert!(!output.replace("\r\n", "").contains('\n'));
    assert!(output.contains("      - hosts: host-dns   # 局域网主机\r\n"));
}

#[test]
fn layout_errors_name_the_module_and_entry() {
    assert_eq!(
        EditError::from(WriteError::Layout {
            module: "strategy",
            name: Some("default".into()),
        })
        .to_string(),
        "configuration source layout of strategy cannot be edited safely"
    );
}
