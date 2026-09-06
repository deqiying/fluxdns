use std::{collections::HashSet, net::SocketAddr, str::FromStr, time::Duration};

use hickory_proto::{
    op::Query,
    rr::{Name, RecordType},
};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::Result;

/// 独立驱动配置，不属于 FluxDNS 的生产 YAML schema。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub targets: Vec<Target>,
    pub queries: Vec<QuerySpec>,
    pub phases: Vec<Phase>,
    pub cycles: u32,
    pub concurrency: usize,
    pub timeout_ms: u64,
    pub sample_interval_ms: u64,
    pub max_errors: u64,
    pub max_scheduler_lag_ms: u64,
    pub reuse_connections: bool,
    pub seed: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub alias: String,
    pub protocol: Protocol,
    pub address: String,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Udp,
    Tcp,
    DohGet,
    DohPost,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySpec {
    pub name: String,
    pub record_type: String,
    pub expected_rcode: u16,
    pub min_answers: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    pub alias: String,
    pub duration_ms: u64,
    pub qps: u32,
}

pub struct PreparedQuery {
    pub question: Query,
    pub expected_rcode: u16,
    pub min_answers: usize,
}

pub enum Endpoint {
    Udp(SocketAddr),
    Tcp(SocketAddr),
    Doh(Url),
}

impl Target {
    pub fn endpoint(&self) -> Result<Endpoint> {
        match self.protocol {
            Protocol::Udp | Protocol::Tcp => {
                let address: SocketAddr = self
                    .address
                    .parse()
                    .map_err(|_| "UDP/TCP address 必须为 IP:port，不做隐式域名解析")?;
                if address.port() == 0
                    || address.ip().is_unspecified()
                    || address.ip().is_multicast()
                {
                    return Err("目标必须是非零端口的单播地址".into());
                }
                Ok(if matches!(self.protocol, Protocol::Udp) {
                    Endpoint::Udp(address)
                } else {
                    Endpoint::Tcp(address)
                })
            }
            Protocol::DohGet | Protocol::DohPost => {
                let url = Url::parse(&self.address).map_err(|_| "DoH URL 无效")?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                    || url.query().is_some()
                    || url.port() == Some(0)
                {
                    return Err(
                        "DoH URL 只允许 HTTP/HTTPS，不支持凭据、fragment 或预置 query".into(),
                    );
                }
                Ok(Endpoint::Doh(url))
            }
        }
    }
}

impl Config {
    /// 在建立 socket、HTTP client 和报告目录前拒绝不完整或无界的运行参数。
    pub fn validate(&self) -> Result<Vec<PreparedQuery>> {
        if self.targets.is_empty() || self.targets.len() > 16 {
            return Err("targets 数量必须为 1..16".into());
        }
        if self.queries.is_empty() || self.queries.len() > 4096 {
            return Err("queries 数量必须为 1..4096".into());
        }
        if self.phases.is_empty() || self.phases.len() > 64 || !(1..=100).contains(&self.cycles) {
            return Err("phases 数量必须为 1..64，cycles 必须为 1..100".into());
        }
        if !(1..=4096).contains(&self.concurrency) || self.concurrency * self.targets.len() > 4096 {
            return Err("concurrency 与 target 数量的乘积必须为 1..4096".into());
        }
        if !(1..=60_000).contains(&self.timeout_ms)
            || !(100..=60_000).contains(&self.sample_interval_ms)
            || !(1..=60_000).contains(&self.max_scheduler_lag_ms)
        {
            return Err("请求/调度超时必须为 1..60000ms，采样间隔为 100..60000ms".into());
        }
        let mut aliases = HashSet::new();
        for target in &self.targets {
            validate_alias(&target.alias)?;
            if !aliases.insert(&target.alias) {
                return Err("target alias 不得重复".into());
            }
            target.endpoint()?;
        }
        let mut duration = 0_u64;
        for phase in &self.phases {
            validate_alias(&phase.alias)?;
            if !(1..=1_000_000).contains(&phase.qps)
                || !(1..=86_400_000).contains(&phase.duration_ms)
            {
                return Err("phase qps 必须为 1..1000000，duration_ms 为 1..86400000".into());
            }
            duration += phase.duration_ms;
        }
        if duration * u64::from(self.cycles) > 86_400_000 {
            return Err("总发送时长不得超过 24 小时".into());
        }
        self.queries
            .iter()
            .map(|query| {
                if query.expected_rcode > 4095 || query.min_answers > 65535 {
                    return Err("expected_rcode 或 min_answers 超出 DNS 范围".into());
                }
                let mut name = Name::from_ascii(&query.name).map_err(|_| "查询 name 无效")?;
                name.set_fqdn(true);
                let record_type =
                    RecordType::from_str(&query.record_type).map_err(|_| "record_type 无效")?;
                Ok(PreparedQuery {
                    question: Query::query(name, record_type),
                    expected_rcode: query.expected_rcode,
                    min_answers: query.min_answers,
                })
            })
            .collect()
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty()
        || alias.len() > 48
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("alias 只允许 1..48 个 ASCII 字母、数字、连字符或下划线".into());
    }
    Ok(())
}
