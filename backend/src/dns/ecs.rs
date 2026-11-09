//! 请求级 ECS 事实；不依赖管理协议，也不参与 DNS 决策。
use std::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_ECS_JSON_BYTES: usize = 4096;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryEcs {
    pub request: EcsValue,
    pub upstream: EcsExchange,
    #[serde(deserialize_with = "required_nullable")]
    pub cache_match: Option<EcsCacheMatch>,
    #[serde(deserialize_with = "required_nullable")]
    pub refresh: Option<EcsExchange>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", try_from = "EcsValueWire")]
pub enum EcsValue {
    Present { cidr: String, scope_prefix: u8 },
    Absent,
    Invalid,
    Unrecorded,
}

// serde 的 internally tagged unit variant 会忽略附加字段；空 struct variant 才严格拒绝。
#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum EcsValueWire {
    Present { cidr: String, scope_prefix: u8 },
    Absent {},
    Invalid {},
    Unrecorded {},
}

impl TryFrom<EcsValueWire> for EcsValue {
    type Error = &'static str;
    fn try_from(value: EcsValueWire) -> Result<Self, Self::Error> {
        let value = match value {
            EcsValueWire::Present { cidr, scope_prefix } => Self::Present { cidr, scope_prefix },
            EcsValueWire::Absent {} => Self::Absent,
            EcsValueWire::Invalid {} => Self::Invalid,
            EcsValueWire::Unrecorded {} => Self::Unrecorded,
        };
        if !value.validate() {
            return Err("invalid ECS value");
        }
        Ok(value)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum EcsExchange {
    Observed {
        query: EcsValue,
        response: EcsValue,
        policy: EcsPolicy,
        upstream_leaf_name: String,
    },
    NotSent {
        reason: EcsNotSentReason,
    },
    Unrecorded {
        reason: EcsUnrecordedReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcsPolicy {
    pub source: EcsPolicySource,
    pub mode: EcsPolicyMode,
    pub value_source: EcsValueSource,
}

macro_rules! ecs_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}
ecs_enum!(EcsPolicySource {
    Default,
    Global,
    Upstream,
    Client,
    Strategy,
    Rule
});
ecs_enum!(EcsPolicyMode {
    Disabled,
    Client,
    Custom
});
ecs_enum!(EcsValueSource {
    ClientEcs,
    ClientIp,
    Custom,
    None
});
ecs_enum!(EcsNotSentReason {
    CacheHit,
    LocalAnswer,
    SingleFlight,
    NotStarted
});
ecs_enum!(EcsUnrecordedReason {
    NotObserved,
    NoSelectedResponse,
    Deadline,
    Budget
});
ecs_enum!(EcsCacheMatchKind {
    Covering,
    Exact,
    NoEcs,
    Members
});

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcsCacheMatch {
    pub kind: EcsCacheMatchKind,
    #[serde(deserialize_with = "required_nullable")]
    pub cidr: Option<String>,
}

// Option 的普通 derive 接受缺字段；ECS 对象要求显式 null，防止残缺快照被接受。
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
/// 错误只包含静态分类，不携带地址、名称或 serde 错误正文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EcsCodecError {
    Invalid,
    Budget,
}

/// 仅接受清除 host bits 后的标准 CIDR，避免诊断泄漏主机地址。
pub fn valid_cidr(cidr: &str) -> Option<u8> {
    if cidr.len() > 43 {
        return None;
    }
    let net: ipnet::IpNet = cidr.parse().ok()?;
    if net.trunc().to_string() != cidr {
        return None;
    }
    Some(if net.addr().is_ipv4() { 32 } else { 128 })
}

impl EcsValue {
    pub fn validate(&self) -> bool {
        match self {
            Self::Present { cidr, scope_prefix } => {
                valid_cidr(cidr).is_some_and(|max| *scope_prefix <= max)
            }
            _ => true,
        }
    }
}
impl EcsExchange {
    pub fn validate(&self) -> bool {
        match self {
            Self::Observed {
                query,
                response,
                upstream_leaf_name,
                ..
            } => {
                query.validate()
                    && response.validate()
                    && !upstream_leaf_name.is_empty()
                    && upstream_leaf_name.len() <= 128
                    && !upstream_leaf_name.chars().any(char::is_control)
            }
            _ => true,
        }
    }
}
impl EcsCacheMatch {
    pub fn validate(&self) -> bool {
        match self.kind {
            EcsCacheMatchKind::Covering | EcsCacheMatchKind::Exact => {
                self.cidr.as_deref().and_then(valid_cidr).is_some()
            }
            EcsCacheMatchKind::NoEcs | EcsCacheMatchKind::Members => self.cidr.is_none(),
        }
    }
}
impl QueryEcs {
    pub fn validate(&self) -> bool {
        self.request.validate()
            && self.upstream.validate()
            && self
                .cache_match
                .as_ref()
                .is_none_or(EcsCacheMatch::validate)
            && self.refresh.as_ref().is_none_or(EcsExchange::validate)
    }

    /// 后台编码边界：拒绝非法内容和超预算；调用方仅降级 ECS。
    pub fn encode_json(&self) -> Result<String, EcsCodecError> {
        if !self.validate() {
            return Err(EcsCodecError::Invalid);
        }
        let text = serde_json::to_string(self).map_err(|_| EcsCodecError::Invalid)?;
        if text.len() > MAX_ECS_JSON_BYTES {
            return Err(EcsCodecError::Budget);
        }
        Ok(text)
    }

    /// 历史解码边界：先限制字节，再解析和校验，不回显输入。
    pub fn decode_json(text: &str) -> Result<Self, EcsCodecError> {
        if text.len() > MAX_ECS_JSON_BYTES {
            return Err(EcsCodecError::Budget);
        }
        let value: Self = serde_json::from_str(text).map_err(|_| EcsCodecError::Invalid)?;
        if !value.validate() {
            return Err(EcsCodecError::Invalid);
        }
        Ok(value)
    }
}

// 即使包含恶意名称，Debug 也不能输出请求详情。
macro_rules! safe_debug {
    ($($ty:ty),+) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "(<redacted>)"))
        }
    })+ };
}
safe_debug!(QueryEcs, EcsValue, EcsExchange, EcsCacheMatch);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_and_scope_are_strict() {
        assert!(valid_cidr("203.0.113.0/24").is_some());
        assert!(valid_cidr("2001:db8::/32").is_some());
        assert!(valid_cidr("::ffff:192.0.2.0/120").is_some());
        for value in [
            "203.0.113.1/24",
            "2001:db8::1/32",
            "192.0.2.0/33",
            "2001:db8::/129",
        ] {
            assert!(valid_cidr(value).is_none());
        }
        assert!(
            !EcsValue::Present {
                cidr: "203.0.113.0/24".into(),
                scope_prefix: 33
            }
            .validate()
        );
    }

    #[test]
    fn codec_budget_and_debug_are_safe() {
        let value = QueryEcs {
            request: EcsValue::Present {
                cidr: "2001:db8::/32".into(),
                scope_prefix: 128,
            },
            upstream: EcsExchange::NotSent {
                reason: EcsNotSentReason::CacheHit,
            },
            cache_match: None,
            refresh: None,
        };
        let encoded = value.encode_json().unwrap();
        assert_eq!(QueryEcs::decode_json(&encoded).unwrap(), value);
        let padded = format!(
            "{encoded}{}",
            " ".repeat(MAX_ECS_JSON_BYTES - encoded.len())
        );
        assert_eq!(QueryEcs::decode_json(&padded).unwrap(), value);
        assert_eq!(
            QueryEcs::decode_json(&(padded + " ")),
            Err(EcsCodecError::Budget)
        );
        assert!(!format!("{value:?}").contains("2001:db8"));
        assert_eq!(
            QueryEcs::decode_json(&" ".repeat(MAX_ECS_JSON_BYTES + 1)),
            Err(EcsCodecError::Budget)
        );
        assert!(
            QueryEcs::decode_json(&encoded.replace("\"scope_prefix\":128", "\"scope_prefix\":129"))
                .is_err()
        );
        assert!(
            QueryEcs::decode_json(&encoded.replace("\"request\":", "\"unknown\":0,\"request\":"))
                .is_err()
        );
    }

    #[test]
    fn maximum_escaped_leaf_names_remain_bounded_and_never_enter_debug() {
        let name = "\\\"".repeat(64);
        let exchange = EcsExchange::Observed {
            query: EcsValue::Present {
                cidr: "2001:db8::/32".into(),
                scope_prefix: 0,
            },
            response: EcsValue::Present {
                cidr: "2001:db8::/32".into(),
                scope_prefix: 128,
            },
            policy: EcsPolicy {
                source: EcsPolicySource::Upstream,
                mode: EcsPolicyMode::Client,
                value_source: EcsValueSource::ClientEcs,
            },
            upstream_leaf_name: name.clone(),
        };
        let value = QueryEcs {
            request: EcsValue::Absent,
            upstream: exchange.clone(),
            cache_match: None,
            refresh: Some(exchange),
        };
        let encoded = value.encode_json().unwrap();
        assert!(encoded.len() <= MAX_ECS_JSON_BYTES);
        assert_eq!(QueryEcs::decode_json(&encoded).unwrap(), value);
        assert!(!format!("{value:?}").contains(&name));
        let mut invalid = value;
        if let EcsExchange::Observed {
            upstream_leaf_name, ..
        } = &mut invalid.upstream
        {
            upstream_leaf_name.push('x');
        }
        assert_eq!(invalid.encode_json(), Err(EcsCodecError::Invalid));
    }
}
