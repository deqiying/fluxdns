//! Cache key 的稳定、无客户端明文编码。
//!
//! v3 的答案 key 只由会改变上游答案的因素组成：namespace、transport 兼容性、target 子树摘要、
//! question 与相关 flags、规范化后保留的 EDNS option，以及各成员最终 ECS。客户端、策略、listener、
//! DoH route、规则与 hosts 只参与决策，不进入 key。
//!
//! 统一 ECS 的答案按 RFC 7871 §7.3.1 以应答 scope 绑定网段：存储 key 在 base 之后追加
//! `前缀长度 | 网段 | tag`，lookup 按最长前缀探测；无 ECS 与异构成员组合只做精确匹配。

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use hickory_proto::rr::rdata::opt::{ClientSubnet, EdnsCode};
use hickory_proto::serialize::binary::BinEncodable;
use sha2::{Digest, Sha256};

use crate::dns::{CacheCompatibilityKey, CanonicalQuery};
use crate::ports::cache::{CacheKey, CacheNamespace};

pub const CACHE_KEY_FORMAT_VERSION: u16 = 3;

/// 无 ECS 或异构成员组合：base 即完整身份，只精确匹配。
const TAG_WHOLE: u8 = 0x00;
/// 统一 ECS 的 covering 条目：服务落在该网段内、且查询 source 不短于该前缀的请求。
const TAG_COVERING_V4: u8 = 0x14;
const TAG_COVERING_V6: u8 = 0x16;
/// 统一 ECS 的 exact 条目：只服务 source 与网段完全相同的请求。
const TAG_EXACT_V4: u8 = 0x24;
const TAG_EXACT_V6: u8 = 0x26;

/// 由 policy/resource 层产生的 opaque 32-byte fingerprint。
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CacheFingerprint([u8; 32]);

impl CacheFingerprint {
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for CacheFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CacheFingerprint(REDACTED)")
    }
}

/// 答案 key 的 ECS 维度。
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum AnswerEcs {
    /// 最终查询不带 ECS：独立的无 ECS 桶，不服务带 ECS 的请求。
    Absent,
    /// 所有成员最终 ECS 一致：按上游应答 scope 参与最长前缀匹配。
    Uniform(ClientSubnet),
    /// 成员最终 ECS 不一致：按成员组合摘要精确匹配，不参与 scope 匹配。
    Members(CacheFingerprint),
}

impl AnswerEcs {
    /// 由统一的最终查询 ECS 构造维度；调用方保证网段已清除 host bits。
    pub fn from_query(subnet: Option<ClientSubnet>) -> Self {
        subnet.map_or(Self::Absent, |subnet| {
            Self::Uniform(ClientSubnet::new(subnet.addr(), subnet.source_prefix(), 0))
        })
    }

    /// 由 `(成员 ID, 最终 ECS)` 列表构造异构组合摘要；调用方须按成员 ID 排序传入。
    pub fn members<'a>(members: impl IntoIterator<Item = (&'a str, Option<ClientSubnet>)>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"fluxdns/cache-member-ecs/v3\0");
        for (member, subnet) in members {
            hasher.update((member.len() as u64).to_be_bytes());
            hasher.update(member.as_bytes());
            match subnet {
                None => hasher.update([0]),
                Some(subnet) => {
                    let mut encoded = Vec::with_capacity(18);
                    push_address(&mut encoded, subnet.addr());
                    encoded.push(subnet.source_prefix());
                    hasher.update([1]);
                    hasher.update(encoded);
                }
            }
        }
        Self::Members(CacheFingerprint::from_digest(hasher.finalize().into()))
    }
}

/// 去掉 ECS 地址后的答案身份与最终 ECS 维度。
///
/// 回源前只能确定 base 与查询 ECS；应答到达后再由 [`AnswerKey::storage_key`] 按 scope 得到
/// 实际存储 key。相等比较即“决策是否仍产生相同的上游请求”。
#[derive(Clone, Eq, PartialEq)]
pub struct AnswerKey {
    namespace: CacheNamespace,
    base: Arc<[u8]>,
    ecs: AnswerEcs,
}

impl fmt::Debug for AnswerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnswerKey")
            .field("namespace", &self.namespace)
            .field("base_len", &self.base.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheKeyError {
    QueryEncoding,
    ComponentTooLarge,
}

/// 构造答案 key；`query` 只读取 question、flags 与非 ECS 的 EDNS option。
pub fn build_answer_key(
    namespace: CacheNamespace,
    query: &CanonicalQuery,
    transport: CacheCompatibilityKey,
    target: CacheFingerprint,
    ecs: AnswerEcs,
) -> Result<AnswerKey, CacheKeyError> {
    let mut encoded = Vec::with_capacity(96);
    encoded.extend_from_slice(b"FDCK");
    encoded.extend_from_slice(&CACHE_KEY_FORMAT_VERSION.to_be_bytes());
    encode_namespace(&mut encoded, &namespace)?;
    encoded.extend_from_slice(&transport.value().to_be_bytes());
    encoded.extend_from_slice(&target.as_bytes());
    encode_query(&mut encoded, query)?;
    match ecs {
        AnswerEcs::Absent => encoded.push(0),
        AnswerEcs::Uniform(subnet) => encoded.push(match subnet.addr() {
            IpAddr::V4(_) => 4,
            IpAddr::V6(_) => 6,
        }),
        AnswerEcs::Members(digest) => {
            encoded.push(0xFF);
            encoded.extend_from_slice(&digest.as_bytes());
        }
    }
    Ok(AnswerKey {
        namespace,
        base: Arc::from(encoded),
        ecs,
    })
}

/// 探测候选的匹配语义与 key 同源生成；地址仅在受保护详情投影时格式化。
#[derive(Clone)]
pub(crate) struct AnswerProbeMatch {
    kind: crate::dns::ecs::EcsCacheMatchKind,
    network: Option<(IpAddr, u8)>,
}

impl AnswerProbeMatch {
    pub(crate) fn observation(&self) -> crate::dns::ecs::EcsCacheMatch {
        crate::dns::ecs::EcsCacheMatch {
            kind: self.kind,
            cidr: self
                .network
                .map(|(address, prefix)| format!("{address}/{prefix}")),
        }
    }
}

impl AnswerKey {
    /// single-flight key：回源前尚不知道 scope，按 base 与查询 ECS 精确值合并。
    pub fn load_key(&self) -> CacheKey {
        match self.ecs {
            AnswerEcs::Uniform(subnet) => self.subnet_key(
                exact_tag(subnet.addr()),
                subnet.addr(),
                subnet.source_prefix(),
            ),
            AnswerEcs::Absent | AnswerEcs::Members(_) => self.whole_key(),
        }
    }

    /// 按 lookup 顺序返回需要探测的存储 key。
    ///
    /// `covering_prefixes` 为该 base 下已存在 covering 条目的前缀长度（升序）；只探测不超过
    /// 查询 source 的长度，从长到短，最后探测 source 本身的 exact 条目。
    pub fn probe_keys(&self, covering_prefixes: &[u8]) -> Vec<CacheKey> {
        self.probe_candidates(covering_prefixes, false)
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    }

    /// 不改变原有候选顺序；仅在详情采集开启时构造绑定的地址匹配描述。
    pub(crate) fn probe_candidates(
        &self,
        covering_prefixes: &[u8],
        observe: bool,
    ) -> Vec<(CacheKey, Option<AnswerProbeMatch>)> {
        use crate::dns::ecs::EcsCacheMatchKind;
        let AnswerEcs::Uniform(subnet) = self.ecs else {
            return vec![(
                self.whole_key(),
                if observe {
                    Some(AnswerProbeMatch {
                        kind: if matches!(self.ecs, AnswerEcs::Members(_)) {
                            EcsCacheMatchKind::Members
                        } else {
                            EcsCacheMatchKind::NoEcs
                        },
                        network: None,
                    })
                } else {
                    None
                },
            )];
        };
        let source = subnet.source_prefix();
        covering_prefixes
            .iter()
            .rev()
            .filter(|prefix| **prefix <= source)
            .map(|prefix| {
                (
                    self.subnet_key(covering_tag(subnet.addr()), subnet.addr(), *prefix),
                    observe.then(|| AnswerProbeMatch {
                        kind: EcsCacheMatchKind::Covering,
                        network: Some((masked(subnet.addr(), *prefix), *prefix)),
                    }),
                )
            })
            .chain(std::iter::once((
                self.subnet_key(exact_tag(subnet.addr()), subnet.addr(), source),
                observe.then(|| AnswerProbeMatch {
                    kind: EcsCacheMatchKind::Exact,
                    network: Some((masked(subnet.addr(), source), source)),
                }),
            )))
            .collect()
    }

    /// 按上游应答 ECS 计算存储 key（RFC 7871 §7.3.1）。
    ///
    /// - 应答 scope `C ≤ S`：covering `/C`；
    /// - `C > S` 且 `S` 已达隐私上限（IPv4 24、IPv6 56）：covering `/S`；
    /// - `C > S` 且 `S` 未达上限：exact `/S`，只服务 source 相同的请求；
    /// - 应答不带 ECS，或 FAMILY/SOURCE/ADDRESS 与查询不一致：忽略应答 ECS，视为 `C = S`。
    ///   不一致应答仍正常写入与返回，这是对 RFC “MUST drop” 的有意偏离，共享范围不超过查询网段；
    /// - 查询 source 为 0：exact `/0`，与因应答 scope 0 产生的 covering `/0` 区分。
    pub fn storage_key(&self, response_ecs: Option<ClientSubnet>) -> CacheKey {
        let AnswerEcs::Uniform(query) = self.ecs else {
            return self.whole_key();
        };
        let address = query.addr();
        let source = query.source_prefix();
        if source == 0 {
            return self.subnet_key(exact_tag(address), address, 0);
        }
        let scope = match response_ecs {
            Some(response) if same_query_subnet(response, query) => response.scope_prefix(),
            Some(_) => {
                tracing::debug!(
                    operation = "cache_scope",
                    reason = "ecs_mismatch",
                    "应答 ECS 与查询不一致，按查询 source 写入缓存"
                );
                source
            }
            None => source,
        };
        if scope <= source {
            self.subnet_key(covering_tag(address), address, scope)
        } else if source >= ecs_privacy_prefix(address) {
            self.subnet_key(covering_tag(address), address, source)
        } else {
            self.subnet_key(exact_tag(address), address, source)
        }
    }

    fn whole_key(&self) -> CacheKey {
        let mut encoded = Vec::with_capacity(self.base.len() + 1);
        encoded.extend_from_slice(&self.base);
        encoded.push(TAG_WHOLE);
        self.cache_key(encoded)
    }

    fn subnet_key(&self, tag: u8, address: IpAddr, prefix: u8) -> CacheKey {
        let mut encoded = Vec::with_capacity(self.base.len() + 18);
        encoded.extend_from_slice(&self.base);
        encoded.push(prefix);
        push_address(&mut encoded, masked(address, prefix));
        encoded.push(tag);
        self.cache_key(encoded)
    }

    fn cache_key(&self, encoded: Vec<u8>) -> CacheKey {
        CacheKey {
            namespace: self.namespace.clone(),
            encoded: Arc::from(encoded),
            format_version: CACHE_KEY_FORMAT_VERSION,
        }
    }

    pub(super) fn base(&self) -> &[u8] {
        &self.base
    }
}

/// 解析 v3 covering 存储 key，返回 `(base, 前缀长度)`；其他 key 返回 `None`。
///
/// store 据此维护前缀长度索引，快照恢复同样经由插入路径，因此索引无需单独持久化。
pub(super) fn covering_prefix(key: &CacheKey) -> Option<(&[u8], u8)> {
    if key.format_version != CACHE_KEY_FORMAT_VERSION {
        return None;
    }
    let (tag, rest) = key.encoded.split_last()?;
    let address_len = match *tag {
        TAG_COVERING_V4 => 4,
        TAG_COVERING_V6 => 16,
        _ => return None,
    };
    let prefix_at = rest.len().checked_sub(address_len + 1)?;
    Some((&rest[..prefix_at], rest[prefix_at]))
}

/// 新 covering 条目是否完整覆盖旧条目；exact 不扩大适用范围。
pub(super) fn covers(new: &CacheKey, old: &CacheKey) -> bool {
    let Some((base, prefix)) = covering_prefix(new) else {
        return false;
    };
    if old.format_version != CACHE_KEY_FORMAT_VERSION
        || old.encoded.len() != new.encoded.len()
        || !old.encoded.starts_with(base)
    {
        return false;
    }
    let offset = base.len();
    let old_prefix = old.encoded[offset];
    let tag = *old.encoded.last().unwrap();
    if old_prefix < prefix
        || !matches!(
            tag,
            TAG_COVERING_V4 | TAG_COVERING_V6 | TAG_EXACT_V4 | TAG_EXACT_V6
        )
    {
        return false;
    }
    let bytes = usize::from(prefix / 8);
    let bits = prefix % 8;
    let start = offset + 1;
    new.encoded[start..start + bytes] == old.encoded[start..start + bytes]
        && (bits == 0
            || (new.encoded[start + bytes] ^ old.encoded[start + bytes]) >> (8 - bits) == 0)
}

/// 客户端 ECS 的隐私前缀上限：IPv4 `/24`、IPv6 `/56`。
pub(crate) const fn ecs_privacy_prefix(address: IpAddr) -> u8 {
    match address {
        IpAddr::V4(_) => 24,
        IpAddr::V6(_) => 56,
    }
}

const fn covering_tag(address: IpAddr) -> u8 {
    match address {
        IpAddr::V4(_) => TAG_COVERING_V4,
        IpAddr::V6(_) => TAG_COVERING_V6,
    }
}

const fn exact_tag(address: IpAddr) -> u8 {
    match address {
        IpAddr::V4(_) => TAG_EXACT_V4,
        IpAddr::V6(_) => TAG_EXACT_V6,
    }
}

/// 应答 ECS 的 FAMILY、SOURCE 与 ADDRESS 前 SOURCE 位是否与查询一致。
fn same_query_subnet(response: ClientSubnet, query: ClientSubnet) -> bool {
    let source = query.source_prefix();
    response.addr().is_ipv4() == query.addr().is_ipv4()
        && response.source_prefix() == source
        && masked(response.addr(), source) == masked(query.addr(), source)
}

fn masked(address: IpAddr, prefix: u8) -> IpAddr {
    match address {
        IpAddr::V4(address) => {
            let mask = u32::MAX
                .checked_shl(32 - u32::from(prefix.min(32)))
                .unwrap_or(0);
            IpAddr::from((u32::from(address) & mask).to_be_bytes())
        }
        IpAddr::V6(address) => {
            let mask = u128::MAX
                .checked_shl(128 - u32::from(prefix.min(128)))
                .unwrap_or(0);
            IpAddr::from((u128::from(address) & mask).to_be_bytes())
        }
    }
}

fn push_address(output: &mut Vec<u8>, address: IpAddr) {
    match address {
        IpAddr::V4(address) => output.extend_from_slice(&address.octets()),
        IpAddr::V6(address) => output.extend_from_slice(&address.octets()),
    }
}

/// 编码 question、RD/CD/DO flags 与保留的 EDNS option。
///
/// ECS 由 ECS 维度单独表达；COOKIE、PADDING、TCP keepalive 与 UDP payload size 不改变答案，
/// 不进入 key。未知 option 保守保留，排序后编码。
fn encode_query(output: &mut Vec<u8>, query: &CanonicalQuery) -> Result<(), CacheKeyError> {
    let question = query.question();
    let name = question
        .name()
        .to_bytes()
        .map_err(|_| CacheKeyError::QueryEncoding)?;
    encode_component(output, &name)?;
    output.extend_from_slice(&u16::from(question.query_type()).to_be_bytes());
    output.extend_from_slice(&u16::from(question.query_class()).to_be_bytes());

    let message = query.as_message();
    let edns = message.edns.as_ref();
    let flags = u8::from(message.metadata.recursion_desired)
        | u8::from(message.metadata.checking_disabled) << 1
        | u8::from(edns.is_some_and(|edns| edns.flags().dnssec_ok)) << 2;
    output.push(flags);

    let mut options = Vec::new();
    for (code, option) in edns.map_or(&[][..], |edns| edns.options().as_ref()) {
        if matches!(
            code,
            EdnsCode::Subnet | EdnsCode::Cookie | EdnsCode::Padding | EdnsCode::Keepalive
        ) {
            continue;
        }
        let data = Vec::<u8>::try_from(option).map_err(|_| CacheKeyError::QueryEncoding)?;
        options.push((u16::from(*code), data));
    }
    options.sort();
    let count = u16::try_from(options.len()).map_err(|_| CacheKeyError::ComponentTooLarge)?;
    output.extend_from_slice(&count.to_be_bytes());
    for (code, data) in options {
        output.extend_from_slice(&code.to_be_bytes());
        encode_component(output, &data)?;
    }
    Ok(())
}

fn encode_namespace(output: &mut Vec<u8>, namespace: &CacheNamespace) -> Result<(), CacheKeyError> {
    match namespace {
        CacheNamespace::Global => output.push(0),
        CacheNamespace::Strategy(strategy) => {
            output.push(1);
            encode_component(output, strategy.as_bytes())?;
        }
        CacheNamespace::ClientStrategy {
            client_digest,
            strategy,
        } => {
            output.push(2);
            output.extend_from_slice(&client_digest.as_bytes());
            encode_component(output, strategy.as_bytes())?;
        }
    }
    Ok(())
}

fn encode_component(output: &mut Vec<u8>, component: &[u8]) -> Result<(), CacheKeyError> {
    let length = u32::try_from(component.len()).map_err(|_| CacheKeyError::ComponentTooLarge)?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(component);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::str::FromStr;

    use hickory_proto::op::{Edns, Message, MessageType, OpCode, Query};
    use hickory_proto::rr::rdata::opt::{ClientSubnet, EdnsOption};
    use hickory_proto::rr::{Name, RecordType};

    use crate::dns::{CacheCompatibilityKey, CanonicalQuery};
    use crate::ports::cache::{CacheKey, CacheNamespace, CacheStrategyId, ClientCacheDigest};

    use super::{
        AnswerEcs, AnswerKey, CACHE_KEY_FORMAT_VERSION, CacheFingerprint, build_answer_key,
        covering_prefix,
    };

    fn message(id: u16) -> Message {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.metadata.recursion_desired = true;
        message.add_query(Query::query(
            Name::from_str("Example.COM.").unwrap(),
            RecordType::A,
        ));
        message
    }

    fn query(message: Message) -> CanonicalQuery {
        CanonicalQuery::from_message(message).unwrap()
    }

    fn with_edns(edit: impl FnOnce(&mut Edns)) -> CanonicalQuery {
        let mut message = message(1);
        let mut edns = Edns::new();
        edit(&mut edns);
        message.set_edns(edns);
        query(message)
    }

    fn subnet(cidr: &str) -> ClientSubnet {
        let network: ipnet::IpNet = cidr.parse().unwrap();
        ClientSubnet::new(network.network(), network.prefix_len(), 0)
    }

    fn answer(query: &CanonicalQuery, ecs: AnswerEcs) -> AnswerKey {
        build_answer_key(
            CacheNamespace::Global,
            query,
            CacheCompatibilityKey(1),
            CacheFingerprint::from_digest([0x33; 32]),
            ecs,
        )
        .unwrap()
    }

    fn uniform(cidr: &str) -> AnswerKey {
        answer(
            &query(message(1)),
            AnswerEcs::from_query(Some(subnet(cidr))),
        )
    }

    fn response(cidr: &str, scope: u8) -> Option<ClientSubnet> {
        let source = subnet(cidr);
        Some(ClientSubnet::new(
            source.addr(),
            source.source_prefix(),
            scope,
        ))
    }

    #[test]
    fn dns_id_cookie_padding_keepalive_and_udp_size_do_not_change_the_key() {
        let plain = answer(&with_edns(|_| {}), AnswerEcs::Absent);
        assert_eq!(
            answer(&query(message(65535)), AnswerEcs::Absent),
            answer(&query(message(1)), AnswerEcs::Absent)
        );
        let noisy = with_edns(|edns| {
            edns.set_max_payload(4096);
            for code in [10_u16, 11, 12] {
                edns.options_mut()
                    .insert(EdnsOption::Unknown(code, vec![code as u8; 8]));
            }
        });
        assert_eq!(answer(&noisy, AnswerEcs::Absent), plain);
        assert_eq!(plain, answer(&query(message(1)), AnswerEcs::Absent));
        assert_eq!(plain.whole_key().format_version, CACHE_KEY_FORMAT_VERSION);
    }

    #[test]
    fn unknown_options_and_answer_flags_change_the_key() {
        let plain = answer(&with_edns(|_| {}), AnswerEcs::Absent);
        let unknown = with_edns(|edns| {
            edns.options_mut()
                .insert(EdnsOption::Unknown(65001, vec![1, 2]));
        });
        let dnssec = with_edns(|edns| {
            edns.set_dnssec_ok(true);
        });
        let mut no_rd = message(1);
        no_rd.metadata.recursion_desired = false;
        let mut cd = message(1);
        cd.metadata.checking_disabled = true;
        for other in [
            answer(&unknown, AnswerEcs::Absent),
            answer(&dnssec, AnswerEcs::Absent),
            answer(&query(no_rd), AnswerEcs::Absent),
            answer(&query(cd), AnswerEcs::Absent),
        ] {
            assert_ne!(other, plain);
            assert_ne!(other.whole_key(), plain.whole_key());
        }
    }

    #[test]
    fn option_order_does_not_change_the_key() {
        let first = with_edns(|edns| {
            edns.options_mut()
                .insert(EdnsOption::Unknown(65001, vec![1]));
            edns.options_mut()
                .insert(EdnsOption::Unknown(65002, vec![2]));
        });
        let second = with_edns(|edns| {
            edns.options_mut()
                .insert(EdnsOption::Unknown(65002, vec![2]));
            edns.options_mut()
                .insert(EdnsOption::Unknown(65001, vec![1]));
        });
        assert_eq!(
            answer(&first, AnswerEcs::Absent),
            answer(&second, AnswerEcs::Absent)
        );
    }

    #[test]
    fn namespace_transport_target_and_ecs_dimensions_are_isolated() {
        let query = query(message(1));
        let base = answer(&query, AnswerEcs::Absent);
        let namespace = build_answer_key(
            CacheNamespace::ClientStrategy {
                client_digest: ClientCacheDigest::from_digest([0x11; 32]),
                strategy: CacheStrategyId::from_validated_config_id("route-a").unwrap(),
            },
            &query,
            CacheCompatibilityKey(1),
            CacheFingerprint::from_digest([0x33; 32]),
            AnswerEcs::Absent,
        )
        .unwrap();
        let transport = build_answer_key(
            CacheNamespace::Global,
            &query,
            CacheCompatibilityKey(2),
            CacheFingerprint::from_digest([0x33; 32]),
            AnswerEcs::Absent,
        )
        .unwrap();
        let target = build_answer_key(
            CacheNamespace::Global,
            &query,
            CacheCompatibilityKey(1),
            CacheFingerprint::from_digest([0x34; 32]),
            AnswerEcs::Absent,
        )
        .unwrap();
        let members = answer(
            &query,
            AnswerEcs::members([
                ("a", Some(subnet("192.0.2.0/24"))),
                ("b", Some(subnet("198.51.100.0/24"))),
            ]),
        );
        let other_members = answer(
            &query,
            AnswerEcs::members([("a", Some(subnet("192.0.2.0/24"))), ("b", None)]),
        );
        let uniform = uniform("192.0.2.0/24");
        let keys = [
            base.whole_key(),
            namespace.whole_key(),
            transport.whole_key(),
            target.whole_key(),
            members.whole_key(),
            other_members.whole_key(),
            uniform.storage_key(None),
            uniform.load_key(),
        ];
        for (index, key) in keys.iter().enumerate() {
            for other in &keys[index + 1..] {
                assert_ne!(key, other);
            }
        }
        // 统一 ECS 的 /0 covering 与无 ECS 桶、source 0 的 exact 条目互不 alias。
        let scope_zero = uniform.storage_key(response("192.0.2.0/24", 0));
        assert_ne!(scope_zero, base.whole_key());
        assert_ne!(scope_zero, uniform_source_zero().storage_key(None));
        assert_eq!(
            members.storage_key(response("192.0.2.0/24", 0)),
            members.whole_key()
        );
    }

    fn uniform_source_zero() -> AnswerKey {
        answer(
            &query(message(1)),
            AnswerEcs::from_query(Some(ClientSubnet::new(IpAddr::from([0, 0, 0, 0]), 0, 0))),
        )
    }

    /// 覆盖方案 §5.4 应答侧表格的每一行。
    #[test]
    fn storage_key_follows_response_scope_matrix() {
        let key = uniform("192.0.2.0/24");
        let covering = |cidr| uniform(cidr).storage_key(None);
        // C ≤ S：covering /C。
        assert_eq!(
            key.storage_key(response("192.0.2.0/24", 16)),
            covering("192.0.0.0/16")
        );
        // scope 0：任意网段的统一 ECS 查询共享同一个 covering /0 条目。
        assert_eq!(
            key.storage_key(response("192.0.2.0/24", 0)),
            uniform("203.0.113.0/24").storage_key(response("203.0.113.0/24", 0))
        );
        // C > S 且 S 已达上限：covering /S。
        assert_eq!(
            key.storage_key(response("192.0.2.0/24", 32)),
            covering("192.0.2.0/24")
        );
        // 不带 ECS：covering /S。
        assert_eq!(key.storage_key(None), covering("192.0.2.0/24"));
        // FAMILY、SOURCE、ADDRESS 不一致：忽略应答 ECS，covering /S。
        for mismatch in [
            response("198.51.100.0/24", 0),
            response("192.0.0.0/16", 0),
            response("2001:db8::/24", 0),
        ] {
            assert_eq!(key.storage_key(mismatch), covering("192.0.2.0/24"));
        }
        // C > S 且 S 未达上限：exact /S，只由同 source 的请求探测到。
        let short = uniform("192.0.0.0/20");
        let exact = short.storage_key(response("192.0.0.0/20", 24));
        assert_eq!(exact, short.load_key());
        assert!(covering_prefix(&exact).is_none());
        assert!(!uniform("192.0.2.0/24").probe_keys(&[20]).contains(&exact));
        assert!(!uniform("192.0.0.0/16").probe_keys(&[]).contains(&exact));
        // IPv6 上限为 /56。
        let v6 = uniform("2001:db8:0:100::/56");
        assert_eq!(
            v6.storage_key(response("2001:db8:0:100::/56", 64)),
            v6.storage_key(None)
        );
        let v6_short = uniform("2001:db8::/48");
        assert_eq!(
            v6_short.storage_key(response("2001:db8::/48", 56)),
            v6_short.load_key()
        );
    }

    #[test]
    fn source_zero_queries_use_a_distinct_exact_entry() {
        let key = uniform_source_zero();
        let stored = key.storage_key(response("0.0.0.0/0", 0));
        assert_eq!(stored, key.load_key());
        assert!(covering_prefix(&stored).is_none());
        // 应答 scope 0 产生的 covering /0 仍可服务 source 0 的查询。
        assert_eq!(key.probe_keys(&[0, 24]).len(), 2);
    }

    #[test]
    fn probe_keys_prefer_longest_covering_then_exact_source() {
        let key = uniform("192.0.2.0/24");
        let wide = key.storage_key(response("192.0.2.0/24", 0));
        let narrow = key.storage_key(response("192.0.2.0/24", 16));
        let probes = key.probe_keys(&[0, 16, 28]);
        assert_eq!(probes, vec![narrow, wide, key.load_key()]);
        // 同一 /16 内的另一 /24 能命中 /16 与 /0 covering 条目，不同 /16 只能命中 /0。
        assert!(
            uniform("192.0.9.0/24")
                .probe_keys(&[16])
                .contains(&probes[0])
        );
        assert!(
            !uniform("192.1.2.0/24")
                .probe_keys(&[16])
                .contains(&probes[0])
        );
        assert!(
            uniform("203.0.113.0/24")
                .probe_keys(&[0])
                .contains(&probes[1])
        );
        assert_eq!(
            answer(&query(message(1)), AnswerEcs::Absent).probe_keys(&[0, 24]),
            vec![answer(&query(message(1)), AnswerEcs::Absent).whole_key()]
        );
    }

    #[test]
    fn probe_observations_are_opt_in_and_preserve_keys() {
        for key in [
            uniform("192.0.2.0/24"),
            uniform("2001:db8:1200::/56"),
            uniform_source_zero(),
            answer(&query(message(1)), AnswerEcs::Absent),
            answer(&query(message(1)), AnswerEcs::members([("leaf", None)])),
        ] {
            let off = key.probe_candidates(&[0, 16, 24, 48, 56], false);
            let on = key.probe_candidates(&[0, 16, 24, 48, 56], true);
            assert!(off.iter().all(|(_, matched)| matched.is_none()));
            assert!(on.iter().all(|(_, matched)| matched.is_some()));
            assert_eq!(
                off.into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
                on.into_iter().map(|(key, _)| key).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn covering_prefix_parses_only_v3_covering_keys() {
        let key = uniform("192.0.2.0/24");
        let covering = key.storage_key(response("192.0.2.0/24", 16));
        assert_eq!(covering_prefix(&covering), Some((key.base(), 16)));
        let v6 = uniform("2001:db8::/56");
        assert_eq!(
            covering_prefix(&v6.storage_key(None)),
            Some((v6.base(), 56))
        );
        assert!(covering_prefix(&key.load_key()).is_none());
        let legacy = CacheKey {
            format_version: 2,
            ..covering
        };
        assert!(covering_prefix(&legacy).is_none());
    }

    #[test]
    fn key_debug_redacts_encoded_material_and_fingerprints() {
        let key = uniform("192.0.2.0/24");
        for debug in [format!("{key:?}"), format!("{:?}", key.storage_key(None))] {
            assert!(!debug.contains("Example"));
            assert!(!debug.contains("192.0.2"));
        }
    }
}
