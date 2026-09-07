//! 外部源的类型化差异；不把只读配置、凭据或无法完整显示的值装进可采用的片段。

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;

use super::{ConfigStore, ErrorCode, error_code, revision};
use crate::config::{
    model::{HostsResourceDto, ListenerDto, RuleSetDto, UpstreamDto},
    store::active::external::ExternalSourceError,
};
use crate::management::contract::{
    ExternalDiff, ExternalResourceDiff, MAX_CHANGES, MAX_EXTERNAL_DIFF_BYTES, ModuleSource,
    Preconditions, StartupSection,
};

/// 调用方先鉴权并在有界后台任务执行；预览不能代替候选校验、改名确认或应用命令。
pub(crate) fn external_diff(store: &ConfigStore) -> Result<ExternalDiff, ErrorCode> {
    let source = store.external_source().map_err(error_code)?;
    let mut result = ExternalDiff {
        expected: Preconditions {
            active_revision: revision(source.expected.active)?,
            observed_file_revision: revision(source.expected.files)?,
        },
        editable: Vec::new(),
        protected_changes: Vec::new(),
        parse_error: None,
    };
    let external = match source.external {
        Ok(config) => config,
        Err(error) => {
            result.parse_error = Some(match error {
                ExternalSourceError::Missing => ErrorCode::NotFound,
                ExternalSourceError::Unreadable => ErrorCode::ServiceUnavailable,
                ExternalSourceError::Oversized => ErrorCode::PayloadTooLarge,
                ExternalSourceError::UnsupportedVersion => ErrorCode::VersionUnsupported,
                ExternalSourceError::Invalid => ErrorCode::ValidationFailed,
            });
            return Ok(result);
        }
    };
    let active = source.active;
    if active.work.path != external.work.path || active.work.rules_path != external.work.rules_path
    {
        result.protected_changes.push(StartupSection::Work);
    }
    if active.database.kind != external.database.kind
        || active.database.path != external.database.path
        || active.database.records_path != external.database.records_path
    {
        result.protected_changes.push(StartupSection::Database);
    }
    if active.webui.enable != external.webui.enable
        || active.webui.address != external.webui.address
        || active.webui.port != external.webui.port
        || active.webui.public_origin != external.webui.public_origin
    {
        result.protected_changes.push(StartupSection::Webui);
    }
    let users = |config: &crate::config::model::WebUiDto| {
        config
            .users
            .iter()
            .map(|user| (user.name.clone(), user.password_hash.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    if users(&active.webui) != users(&external.webui) {
        result
            .protected_changes
            .push(StartupSection::ProtectedCredentials);
    }
    append_resources(
        &mut result,
        &active.listener,
        &external.listener,
        ListenerDto::name,
        ModuleSource::Listener,
    )?;
    append_resources(
        &mut result,
        &active.upstreams,
        &external.upstreams,
        UpstreamDto::name,
        ModuleSource::Upstreams,
    )?;
    append_resources(
        &mut result,
        &active.strategy,
        &external.strategy,
        |value| &value.name,
        ModuleSource::Strategy,
    )?;
    append_resources(
        &mut result,
        &active.hosts,
        &external.hosts,
        HostsResourceDto::name,
        ModuleSource::Hosts,
    )?;
    append_resources(
        &mut result,
        &active.outbound,
        &external.outbound,
        |value| &value.name,
        ModuleSource::Outbound,
    )?;
    append_resources(
        &mut result,
        &active.rule_set,
        &external.rule_set,
        RuleSetDto::name,
        ModuleSource::RuleSet,
    )?;
    append_resources(
        &mut result,
        &active.clients,
        &external.clients,
        |value| &value.name,
        ModuleSource::Clients,
    )?;
    append(
        &mut result,
        Some(&active.dns),
        Some(&external.dns),
        ModuleSource::Dns,
    )?;
    append(
        &mut result,
        Some(&active.statistics),
        Some(&external.statistics),
        ModuleSource::Statistics,
    )?;
    append(
        &mut result,
        Some(&active.logs),
        Some(&external.logs),
        ModuleSource::Logs,
    )?;
    check_response_budget(&result)?;
    Ok(result)
}

fn append_resources<T: Clone + Serialize>(
    result: &mut ExternalDiff,
    active: &[T],
    external: &[T],
    name: fn(&T) -> &str,
    wrap: fn(T) -> ModuleSource,
) -> Result<(), ErrorCode> {
    // 配置完整校验已拒绝重名；排序只稳定管理差异，不改动资源内部的规则/成员顺序。
    let mut pairs = BTreeMap::new();
    for value in active {
        pairs.insert(name(value), (Some(value), None));
    }
    for value in external {
        pairs.entry(name(value)).or_insert((None, None)).1 = Some(value);
    }
    for (before, after) in pairs.into_values() {
        append(result, before, after, wrap)?;
    }
    Ok(())
}

fn append<T: Clone + Serialize>(
    result: &mut ExternalDiff,
    active: Option<&T>,
    external: Option<&T>,
    wrap: fn(T) -> ModuleSource,
) -> Result<(), ErrorCode> {
    let before = active
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| ErrorCode::ServiceUnavailable)?;
    let after = external
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| ErrorCode::ServiceUnavailable)?;
    if before == after {
        return Ok(());
    }
    let entry = ExternalResourceDiff {
        active: active.cloned().map(wrap),
        external: external.cloned().map(wrap),
    };
    for value in entry.active.iter().chain(entry.external.iter()) {
        check_source_bounds(value)?;
    }
    for value in before.iter().chain(after.iter()) {
        check_safe_numbers(value)?;
    }
    if result.editable.len() >= MAX_CHANGES {
        return Err(ErrorCode::PayloadTooLarge);
    }
    result.editable.push(entry);
    Ok(())
}

// 共享 DTO 的整数由 v2 完整校验约束；出口再守住 schema SafeInteger，防止未来裸 u64 扩展漏接。
fn check_safe_numbers(value: &serde_json::Value) -> Result<(), ErrorCode> {
    match value {
        serde_json::Value::Number(value)
            if !value
                .as_u64()
                .is_some_and(|number| number <= 9_007_199_254_740_991) =>
        {
            Err(ErrorCode::ValidationFailed)
        }
        serde_json::Value::Array(values) => values.iter().try_for_each(check_safe_numbers),
        serde_json::Value::Object(values) => values.values().try_for_each(check_safe_numbers),
        _ => Ok(()),
    }
}

fn text_bound(value: &str, maximum: usize) -> Result<(), ErrorCode> {
    if value.chars().count() > maximum {
        Err(ErrorCode::PayloadTooLarge)
    } else {
        Ok(())
    }
}

fn path_bound(value: &Path) -> Result<(), ErrorCode> {
    text_bound(value.to_str().ok_or(ErrorCode::ValidationFailed)?, 4096)
}

/// parser 的输入字节限制不等于每个输出字段满足 schema；这里只检查额外的投影长度界限。
fn check_source_bounds(value: &ModuleSource) -> Result<(), ErrorCode> {
    match value {
        ModuleSource::Listener(ListenerDto::Doh {
            routes, endpoints, ..
        }) => {
            for route in routes {
                text_bound(&route.path, 4096)?;
            }
            for endpoint in endpoints {
                for path in endpoint
                    .tls
                    .certificate_file
                    .iter()
                    .chain(endpoint.tls.private_key_file.iter())
                {
                    path_bound(path)?;
                }
            }
        }
        ModuleSource::Upstreams(UpstreamDto::Doh { address, .. }) => {
            text_bound(address.as_str(), 4096)?
        }
        ModuleSource::Hosts(HostsResourceDto::File { path, .. })
        | ModuleSource::RuleSet(RuleSetDto::File { path, .. }) => path_bound(path)?,
        ModuleSource::RuleSet(RuleSetDto::Remote { url, .. }) => text_bound(url.as_str(), 4096)?,
        ModuleSource::Outbound(value) => {
            if let Some(env) = &value.proxy_url.env {
                text_bound(env, 256)?;
            }
            if let Some(path) = &value.proxy_url.file {
                path_bound(path)?;
            }
        }
        ModuleSource::Dns(value) => {
            if let Some(cache) = &value.cache {
                path_bound(&cache.persistence.path)?;
            }
        }
        ModuleSource::Logs(value) => path_bound(&value.path)?,
        _ => {}
    }
    Ok(())
}

struct JsonBudget {
    remaining: usize,
    exceeded: bool,
}

impl Write for JsonBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exceeded = true;
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn check_response_budget(value: &ExternalDiff) -> Result<(), ErrorCode> {
    let mut budget = JsonBudget {
        remaining: MAX_EXTERNAL_DIFF_BYTES,
        exceeded: false,
    };
    serde_json::to_writer(&mut budget, value).map_err(|_| {
        if budget.exceeded {
            ErrorCode::PayloadTooLarge
        } else {
            ErrorCode::ServiceUnavailable
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn safe_number_guard_checks_nested_fields_without_coercing_large_integers() {
        assert!(check_safe_numbers(&json!({"nested":[0, 9_007_199_254_740_991_u64]})).is_ok());
        for value in [
            json!({"nested":[9_007_199_254_740_992_u64]}),
            json!({"nested":{"count":u64::MAX}}),
            json!(-1),
            json!(1.5),
        ] {
            assert!(matches!(
                check_safe_numbers(&value),
                Err(ErrorCode::ValidationFailed)
            ));
        }
    }

    #[test]
    fn json_budget_counts_escaping_and_utf8_bytes_instead_of_character_count() {
        let value = json!("\n界");
        let encoded = serde_json::to_vec(&value).unwrap();
        let mut exact = JsonBudget {
            remaining: encoded.len(),
            exceeded: false,
        };
        serde_json::to_writer(&mut exact, &value).unwrap();
        assert_eq!(exact.remaining, 0);
        let mut short = JsonBudget {
            remaining: encoded.len() - 1,
            exceeded: false,
        };
        assert!(serde_json::to_writer(&mut short, &value).is_err());
        assert!(short.exceeded);
    }
}
