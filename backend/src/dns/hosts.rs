//! 受限 hosts 文本资源解析。

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;

use hickory_proto::rr::{Name, RecordType};
use thiserror::Error;

const MAX_HOSTS_ENTRIES: usize = 65_536;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum HostsParseError {
    #[error("hosts line {line} is missing an address")]
    MissingAddress { line: usize },
    #[error("hosts line {line} contains an invalid address")]
    InvalidAddress { line: usize },
    #[error("hosts line {line} is missing a name")]
    MissingName { line: usize },
    #[error("hosts line {line} contains an invalid name")]
    InvalidName { line: usize },
    #[error("hosts resource exceeds the entry limit")]
    TooManyEntries,
}

#[derive(Clone, Eq, PartialEq)]
pub struct HostsTable {
    entries: BTreeMap<String, Vec<IpAddr>>,
    entry_count: usize,
}

impl fmt::Debug for HostsTable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostsTable")
            .field("name_count", &self.entries.len())
            .field("address_count", &self.entry_count)
            .finish()
    }
}

impl HostsTable {
    pub fn parse(input: &str) -> Result<Self, HostsParseError> {
        let mut entries = BTreeMap::<String, Vec<IpAddr>>::new();
        let mut entry_count = 0_usize;

        for (line_index, raw_line) in input.lines().enumerate() {
            let line = raw_line
                .split_once('#')
                .map_or(raw_line, |(content, _)| content)
                .trim();
            if line.is_empty() {
                continue;
            }

            let mut fields = line.split_whitespace();
            let address = fields
                .next()
                .ok_or(HostsParseError::MissingAddress {
                    line: line_index + 1,
                })?
                .parse::<IpAddr>()
                .map_err(|_| HostsParseError::InvalidAddress {
                    line: line_index + 1,
                })?;
            let mut name_count = 0_usize;
            for raw_name in fields {
                name_count += 1;
                let name = normalize_name(raw_name).ok_or(HostsParseError::InvalidName {
                    line: line_index + 1,
                })?;
                let addresses = entries.entry(name).or_default();
                if !addresses.contains(&address) {
                    addresses.push(address);
                    entry_count += 1;
                    if entry_count > MAX_HOSTS_ENTRIES {
                        return Err(HostsParseError::TooManyEntries);
                    }
                }
            }
            if name_count == 0 {
                return Err(HostsParseError::MissingName {
                    line: line_index + 1,
                });
            }
        }

        Ok(Self {
            entries,
            entry_count,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn name_count(&self) -> usize {
        self.entries.len()
    }

    pub fn address_count(&self) -> usize {
        self.entry_count
    }

    pub fn lookup(&self, name: &Name, record_type: RecordType) -> Option<Vec<IpAddr>> {
        if !matches!(record_type, RecordType::A | RecordType::AAAA) {
            return None;
        }
        let key = normalize_name(&name.to_ascii())?;
        let addresses = self.entries.get(&key)?;
        let filtered = addresses
            .iter()
            .copied()
            .filter(|address| match record_type {
                RecordType::A => address.is_ipv4(),
                RecordType::AAAA => address.is_ipv6(),
                _ => false,
            })
            .collect::<Vec<_>>();
        (!filtered.is_empty()).then_some(filtered)
    }
}

fn normalize_name(value: &str) -> Option<String> {
    let mut name = Name::from_ascii(value).ok()?;
    name.set_fqdn(true);
    Some(name.to_ascii().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;

    use hickory_proto::rr::{Name, RecordType};

    use super::{HostsParseError, HostsTable};

    #[test]
    fn parses_comments_aliases_and_normalizes_names() {
        let table = HostsTable::parse(
            "# comment\n192.0.2.1 Example.COM alias.example\n2001:db8::1 example.com.\n",
        )
        .unwrap();

        assert_eq!(table.name_count(), 2);
        assert_eq!(table.address_count(), 3);
        let name = Name::from_ascii("EXAMPLE.com").unwrap();
        assert_eq!(
            table.lookup(&name, RecordType::A),
            Some(vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))])
        );
        assert_eq!(
            table.lookup(&name, RecordType::AAAA),
            Some(vec![IpAddr::V6(Ipv6Addr::from_str("2001:db8::1").unwrap())])
        );
    }

    #[test]
    fn deduplicates_repeated_addresses() {
        let table =
            HostsTable::parse("192.0.2.1 example.test example.test\n192.0.2.1 example.test\n")
                .unwrap();

        assert_eq!(table.name_count(), 1);
        assert_eq!(table.address_count(), 1);
    }

    #[test]
    fn rejects_missing_or_invalid_fields_with_line_numbers() {
        assert_eq!(
            HostsTable::parse("192.0.2.1\n"),
            Err(HostsParseError::MissingName { line: 1 })
        );
        assert_eq!(
            HostsTable::parse("not-an-ip example.test\n"),
            Err(HostsParseError::InvalidAddress { line: 1 })
        );
        assert_eq!(
            HostsTable::parse("192.0.2.1 bad/name\n"),
            Err(HostsParseError::InvalidName { line: 1 })
        );
    }

    #[test]
    fn unsupported_query_types_have_no_local_answer() {
        let table = HostsTable::parse("192.0.2.1 example.test\n").unwrap();
        let name = Name::from_ascii("example.test.").unwrap();

        assert_eq!(table.lookup(&name, RecordType::CNAME), None);
    }

    #[test]
    fn debug_does_not_expose_names_or_addresses() {
        let table = HostsTable::parse("192.0.2.1 private.example.test\n").unwrap();
        let debug = format!("{table:?}");

        assert!(!debug.contains("private.example.test"));
        assert!(!debug.contains("192.0.2.1"));
        assert!(debug.contains("name_count: 1"));
    }
}
