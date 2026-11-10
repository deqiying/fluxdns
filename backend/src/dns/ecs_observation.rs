//! ECS 执行事实的旁路提取；不参与 query 改写、响应选择或缓存策略。

use hickory_proto::rr::rdata::opt::{ClientSubnet, EdnsCode, EdnsOption};
use ipnet::IpNet;

use super::ecs::{EcsPolicy, EcsPolicyMode, EcsPolicySource, EcsValue, EcsValueSource};
use crate::config::model::EcsMode;
use crate::config::resolve::{ResolvedEcs, ValueSource};

/// 保留该阶段的 source/scope，只清除 host bits，不应用发送隐私前缀。
fn option_value(option: Option<&EdnsOption>) -> EcsValue {
    match option {
        None => EcsValue::Absent,
        Some(EdnsOption::Subnet(subnet)) => subnet_value(*subnet),
        Some(_) => EcsValue::Invalid,
    }
}

pub(super) fn subnet_value(subnet: ClientSubnet) -> EcsValue {
    let max = if subnet.addr().is_ipv4() { 32 } else { 128 };
    if subnet.source_prefix() > max || subnet.scope_prefix() > max {
        return EcsValue::Invalid;
    }
    match IpNet::new(subnet.addr(), subnet.source_prefix()) {
        Ok(network) => EcsValue::Present {
            cidr: network.trunc().to_string(),
            scope_prefix: subnet.scope_prefix(),
        },
        Err(_) => EcsValue::Invalid,
    }
}

pub(crate) fn query_value(query: &super::CanonicalQuery) -> EcsValue {
    option_value(
        query
            .as_message()
            .edns
            .as_ref()
            .and_then(|edns| edns.option(EdnsCode::Subnet)),
    )
}

pub(crate) fn response_value(response: &super::CanonicalResponse) -> EcsValue {
    option_value(
        response
            .as_message()
            .edns
            .as_ref()
            .and_then(|edns| edns.option(EdnsCode::Subnet)),
    )
}

/// 与 effective_upstream_query 相同分支的来源，不在 API 读取时重新继承配置。
pub(super) fn policy(ecs: &ResolvedEcs, value_source: EcsValueSource) -> EcsPolicy {
    EcsPolicy {
        source: match ecs.source {
            ValueSource::Default => EcsPolicySource::Default,
            ValueSource::Global => EcsPolicySource::Global,
            ValueSource::Upstream => EcsPolicySource::Upstream,
            ValueSource::Client => EcsPolicySource::Client,
            ValueSource::Strategy => EcsPolicySource::Strategy,
            ValueSource::Rule => EcsPolicySource::Rule,
        },
        mode: match ecs.mode {
            EcsMode::Disabled => EcsPolicyMode::Disabled,
            EcsMode::Client => EcsPolicyMode::Client,
            EcsMode::Custom => EcsPolicyMode::Custom,
            EcsMode::ClientOrCustom => EcsPolicyMode::ClientOrCustom,
        },
        value_source,
    }
}
