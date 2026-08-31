//! Client exact-ID 与最长 CIDR 匹配索引。

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use ipnet::IpNet;

use crate::config::resolve::{ConfigId, ResolvedClient};

#[derive(Clone, Eq, PartialEq)]
pub struct ClientRule {
    pub name: ConfigId,
    pub ids: Vec<String>,
    pub ips: Vec<IpNet>,
}

impl fmt::Debug for ClientRule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientRule")
            .field("name", &self.name)
            .field("id_count", &self.ids.len())
            .field("ip_count", &self.ips.len())
            .finish()
    }
}

impl ClientRule {
    pub fn from_resolved(client: &ResolvedClient) -> Self {
        Self {
            name: client.id.clone(),
            ids: client.ids.clone(),
            ips: client.ips.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRuleBuildError {
    EmptyRule,
    DuplicateId,
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

#[derive(Clone, Debug, Default)]
pub struct ClientIndex {
    rules: Vec<Arc<ClientRule>>,
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
            if rule.ids.is_empty() && rule.ips.is_empty() {
                return Err(ClientRuleBuildError::EmptyRule);
            }
            let rule_index = index.rules.len();
            let rule = Arc::new(rule);
            for id in &rule.ids {
                if index.exact_ids.insert(id.clone(), rule_index).is_some() {
                    return Err(ClientRuleBuildError::DuplicateId);
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

    pub fn match_client(
        &self,
        client_id: Option<&str>,
        client_addr: Option<IpAddr>,
    ) -> ClientMatch {
        if let Some(client_id) = client_id
            && let Some(rule_index) = self.exact_ids.get(client_id)
        {
            return ClientMatch::Matched {
                client: Arc::clone(&self.rules[*rule_index]),
                kind: ClientMatchKind::ExactId,
            };
        }

        let Some(client_addr) = client_addr else {
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

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;

    use ipnet::IpNet;

    use crate::config::resolve::ConfigId;

    use super::{ClientIndex, ClientMatch, ClientMatchKind, ClientRule, ClientRuleBuildError};

    fn rule(name: &str, ids: &[&str], ips: &[&str]) -> ClientRule {
        ClientRule {
            name: ConfigId::new(name).unwrap(),
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
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
            ClientRuleBuildError::DuplicateId
        );
    }
}
