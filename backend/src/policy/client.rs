//! Client exact-ID 与最长 CIDR 匹配索引。

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use ipnet::IpNet;
use sha2::{Digest, Sha256};

use crate::config::resolve::{ConfigId, ResolvedClient};
use crate::ports::cache::ClientCacheDigest;
use crate::ports::observation::{ClientMatchObservation, ClientMatchSource};

#[derive(Clone, Eq, PartialEq)]
pub struct ClientRule {
    pub name: ConfigId,
    pub client_ids: Vec<String>,
    pub ips: Vec<IpNet>,
}

impl fmt::Debug for ClientRule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientRule")
            .field("name", &self.name)
            .field("client_id_count", &self.client_ids.len())
            .field("ip_count", &self.ips.len())
            .finish()
    }
}

impl ClientRule {
    pub fn from_resolved(client: &ResolvedClient) -> Self {
        Self {
            name: client.name.clone(),
            client_ids: client.client_ids.clone(),
            ips: client.ips.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRuleBuildError {
    EmptyRule,
    DuplicateName,
    DuplicateClientId,
    DuplicateCidr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientMatchKind {
    ExactId,
    Cidr { prefix_len: u8 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientMatch {
    Matched {
        client: Arc<ClientRule>,
        kind: ClientMatchKind,
    },
    Unknown,
}

impl ClientMatch {
    /// 冻结当次匹配使用的配置身份。v1 多 ID/IP 规则没有唯一历史 ID，等待新版基线退出。
    pub(crate) fn observation(&self, client_id: Option<&str>) -> Option<ClientMatchObservation> {
        match self {
            Self::Matched {
                kind: ClientMatchKind::ExactId,
                ..
            } => client_id.map(|matched_client_id| ClientMatchObservation {
                source: ClientMatchSource::Id,
                matched_client_id: Arc::from(matched_client_id),
            }),
            Self::Matched {
                client,
                kind: ClientMatchKind::Cidr { .. },
            } if client.client_ids.len() == 1 => Some(ClientMatchObservation {
                source: ClientMatchSource::Ip,
                matched_client_id: Arc::from(client.client_ids[0].as_str()),
            }),
            Self::Matched { .. } | Self::Unknown => None,
        }
    }

    /// 根据实际命中的身份类型生成域分隔摘要，避免把客户端原始标识写入缓存键。
    pub(crate) fn cache_digest(
        &self,
        client_id: Option<&str>,
        client_addr: Option<IpAddr>,
    ) -> Option<ClientCacheDigest> {
        let mut hasher = Sha256::new();
        hasher.update(b"fluxdns/client-cache/v1\0");
        match (self, client_id, client_addr) {
            (
                Self::Matched {
                    kind: ClientMatchKind::ExactId,
                    ..
                },
                Some(client_id),
                _,
            ) => {
                hasher.update(b"id\0");
                hasher.update(client_id.as_bytes());
            }
            (
                Self::Matched {
                    kind: ClientMatchKind::Cidr { .. },
                    ..
                },
                _,
                Some(IpAddr::V4(client_addr)),
            ) => {
                hasher.update(b"ipv4\0");
                hasher.update(client_addr.octets());
            }
            (
                Self::Matched {
                    kind: ClientMatchKind::Cidr { .. },
                    ..
                },
                _,
                Some(IpAddr::V6(client_addr)),
            ) => {
                if let Some(client_addr) = client_addr.to_ipv4_mapped() {
                    hasher.update(b"ipv4\0");
                    hasher.update(client_addr.octets());
                } else {
                    hasher.update(b"ipv6\0");
                    hasher.update(client_addr.octets());
                }
            }
            _ => return None,
        }

        let digest = hasher.finalize();
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&digest);
        Some(ClientCacheDigest::from_digest(bytes))
    }
}

#[derive(Clone, Debug, Default)]
pub struct ClientIndex {
    rules: Vec<Arc<ClientRule>>,
    names: HashMap<ConfigId, usize>,
    exact_ids: HashMap<String, usize>,
    cidrs: Vec<CidrEntry>,
}

#[derive(Clone, Copy, Debug)]
struct CidrEntry {
    network: IpNet,
    rule_index: usize,
}

impl ClientIndex {
    pub fn build(
        rules: impl IntoIterator<Item = ClientRule>,
    ) -> Result<Self, ClientRuleBuildError> {
        let mut index = Self::default();
        for rule in rules {
            if rule.client_ids.is_empty() && rule.ips.is_empty() {
                return Err(ClientRuleBuildError::EmptyRule);
            }
            let rule_index = index.rules.len();
            if index.names.insert(rule.name.clone(), rule_index).is_some() {
                return Err(ClientRuleBuildError::DuplicateName);
            }
            let rule = Arc::new(rule);
            for id in &rule.client_ids {
                if index.exact_ids.insert(id.clone(), rule_index).is_some() {
                    return Err(ClientRuleBuildError::DuplicateClientId);
                }
            }
            for network in &rule.ips {
                if index.cidrs.iter().any(|entry| entry.network == *network) {
                    return Err(ClientRuleBuildError::DuplicateCidr);
                }
                index.cidrs.push(CidrEntry {
                    network: *network,
                    rule_index,
                });
            }
            index.rules.push(rule);
        }
        index
            .cidrs
            .sort_by_key(|entry| std::cmp::Reverse(entry.network.prefix_len()));
        Ok(index)
    }

    pub fn from_resolved(clients: &[ResolvedClient]) -> Result<Self, ClientRuleBuildError> {
        Self::build(clients.iter().map(ClientRule::from_resolved))
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 按配置管理键定位客户端，不把 `name` 当作请求身份。
    pub fn get_by_name(&self, name: &ConfigId) -> Option<Arc<ClientRule>> {
        self.names
            .get(name)
            .map(|rule_index| Arc::clone(&self.rules[*rule_index]))
    }

    /// 按请求携带的精确 `client_id` 定位客户端，匹配保持大小写敏感。
    pub fn get_by_client_id(&self, client_id: &str) -> Option<Arc<ClientRule>> {
        self.exact_ids
            .get(client_id)
            .map(|rule_index| Arc::clone(&self.rules[*rule_index]))
    }

    pub fn match_client(
        &self,
        client_id: Option<&str>,
        client_addr: Option<IpAddr>,
    ) -> ClientMatch {
        if let Some(client_id) = client_id
            && let Some(client) = self.get_by_client_id(client_id)
        {
            return ClientMatch::Matched {
                client,
                kind: ClientMatchKind::ExactId,
            };
        }

        let Some(client_addr) = client_addr.map(normalize_client_addr) else {
            return ClientMatch::Unknown;
        };
        self.cidrs
            .iter()
            .find(|entry| entry.network.contains(&client_addr))
            .map_or(ClientMatch::Unknown, |entry| ClientMatch::Matched {
                client: Arc::clone(&self.rules[entry.rule_index]),
                kind: ClientMatchKind::Cidr {
                    prefix_len: entry.network.prefix_len(),
                },
            })
    }
}

fn normalize_client_addr(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        IpAddr::V4(_) => address,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;

    use ipnet::IpNet;

    use crate::config::resolve::ConfigId;

    use super::{ClientIndex, ClientMatch, ClientMatchKind, ClientRule, ClientRuleBuildError};
    use crate::ports::observation::ClientMatchSource;

    fn rule(name: &str, ids: &[&str], ips: &[&str]) -> ClientRule {
        ClientRule {
            name: ConfigId::new(name).unwrap(),
            client_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            ips: ips.iter().map(|ip| IpNet::from_str(ip).unwrap()).collect(),
        }
    }

    #[test]
    fn exact_id_takes_precedence_over_cidr() {
        let index = ClientIndex::build([
            rule("network", &[], &["192.0.2.0/24"]),
            rule("named", &["alice"], &["192.0.2.0/24"]),
        ]);
        assert!(matches!(index, Err(ClientRuleBuildError::DuplicateCidr)));

        let index = ClientIndex::build([
            rule("network", &[], &["192.0.2.0/24"]),
            rule("named", &["alice"], &[]),
        ])
        .unwrap();
        let matched =
            index.match_client(Some("alice"), Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8))));
        assert!(matches!(
            matched,
            ClientMatch::Matched {
                kind: ClientMatchKind::ExactId,
                ..
            }
        ));
    }

    #[test]
    fn cidr_match_uses_longest_prefix_for_each_family() {
        let index = ClientIndex::build([
            rule("broad", &[], &["192.0.2.0/24", "2001:db8::/32"]),
            rule("narrow", &[], &["192.0.2.128/25", "2001:db8:1::/48"]),
        ])
        .unwrap();
        assert!(matches!(
            index.match_client(None, Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 200)))),
            ClientMatch::Matched {
                kind: ClientMatchKind::Cidr { prefix_len: 25 },
                ..
            }
        ));
        assert!(matches!(
            index.match_client(
                None,
                Some(IpAddr::V6(Ipv6Addr::from_str("2001:db8:1::1").unwrap()))
            ),
            ClientMatch::Matched {
                kind: ClientMatchKind::Cidr { prefix_len: 48 },
                ..
            }
        ));
        assert!(matches!(
            index.match_client(None, Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)))),
            ClientMatch::Unknown
        ));
    }

    #[test]
    fn build_rejects_empty_and_duplicate_matchers() {
        assert_eq!(
            ClientIndex::build([rule("empty", &[], &[])]).unwrap_err(),
            ClientRuleBuildError::EmptyRule
        );
        assert_eq!(
            ClientIndex::build([rule("one", &["same"], &[]), rule("two", &["same"], &[]),])
                .unwrap_err(),
            ClientRuleBuildError::DuplicateClientId
        );
        assert_eq!(
            ClientIndex::build([rule("same", &["one"], &[]), rule("same", &["two"], &[])])
                .unwrap_err(),
            ClientRuleBuildError::DuplicateName
        );
    }

    #[test]
    fn management_name_and_request_id_use_separate_indexes() {
        let index = ClientIndex::build([
            rule("desktop", &["Desktop-01"], &[]),
            rule("phone", &["Phone-01"], &[]),
        ])
        .unwrap();

        let desktop_name = ConfigId::new("desktop").unwrap();
        assert_eq!(index.get_by_name(&desktop_name).unwrap().name, desktop_name);
        assert_eq!(
            index.get_by_client_id("Desktop-01").unwrap().name,
            desktop_name
        );
        assert!(index.get_by_client_id("desktop").is_none());
        assert!(index.get_by_client_id("desktop-01").is_none());
    }

    #[test]
    fn mapped_ipv4_address_matches_and_hashes_as_ipv4() {
        let index = ClientIndex::build([rule("office", &[], &["192.0.2.0/24"])]).unwrap();
        let ipv4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8));
        let mapped = IpAddr::V6(Ipv6Addr::from_str("::ffff:192.0.2.8").unwrap());

        let ipv4_match = index.match_client(None, Some(ipv4));
        let mapped_match = index.match_client(None, Some(mapped));
        assert_eq!(ipv4_match, mapped_match);
        assert_eq!(
            ipv4_match.cache_digest(None, Some(ipv4)),
            mapped_match.cache_digest(None, Some(mapped))
        );
    }

    #[test]
    fn cache_digest_uses_the_identity_that_actually_matched() {
        let index = ClientIndex::build([rule("mixed", &["alice"], &["192.0.2.0/24"])]).unwrap();
        let address = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8));

        let exact = index.match_client(Some("alice"), Some(address));
        assert_eq!(
            exact.cache_digest(Some("alice"), Some(address)),
            exact.cache_digest(Some("alice"), None)
        );

        let cidr = index.match_client(Some("unknown-a"), Some(address));
        assert_eq!(
            cidr.cache_digest(Some("unknown-a"), Some(address)),
            cidr.cache_digest(Some("unknown-b"), Some(address))
        );
        assert_ne!(
            exact.cache_digest(Some("alice"), Some(address)),
            cidr.cache_digest(Some("alice"), Some(address))
        );
    }

    #[test]
    fn historical_match_freezes_id_or_ip_source_without_using_management_name() {
        let index = ClientIndex::build([rule(
            "mutable-name",
            &["Stable-Client-01"],
            &["192.0.2.0/24"],
        )])
        .unwrap();
        let address = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8));

        let exact = index
            .match_client(Some("Stable-Client-01"), Some(address))
            .observation(Some("Stable-Client-01"))
            .unwrap();
        assert_eq!(exact.source, ClientMatchSource::Id);
        assert_eq!(exact.matched_client_id.as_ref(), "Stable-Client-01");

        let cidr = index
            .match_client(Some("unknown"), Some(address))
            .observation(Some("unknown"))
            .unwrap();
        assert_eq!(cidr.source, ClientMatchSource::Ip);
        assert_eq!(cidr.matched_client_id.as_ref(), "Stable-Client-01");
    }
}
