//! A/AAAA/CNAME hosts 资源的规范化、解析和不可变匹配索引。

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use hickory_proto::rr::Name;
use serde::Deserialize;
use thiserror::Error;

const DEFAULT_MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_RECORDS: usize = 65_536;
const DEFAULT_MAX_NAMES: usize = 65_536;
const DEFAULT_MAX_LINE_BYTES: usize = 16 * 1024;

/// 一个已去掉末尾根点、且统一为小写的 DNS name。
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalDomain(String);

impl CanonicalDomain {
    pub fn parse(value: &str) -> Result<Self, HostsParseError> {
        let value = value.trim();
        let name = value.strip_prefix("*.").unwrap_or(value);
        if name.is_empty() || name.contains('*') {
            return Err(HostsParseError::InvalidName);
        }

        let mut parsed = Name::from_ascii(value).map_err(|_| HostsParseError::InvalidName)?;
        parsed.set_fqdn(true);
        let mut normalized = parsed.to_ascii().to_ascii_lowercase();
        if normalized != "." {
            normalized.pop();
        }
        if normalized.is_empty() || normalized == "*" {
            return Err(HostsParseError::InvalidName);
        }
        Ok(Self(normalized))
    }

    pub fn is_wildcard(&self) -> bool {
        self.0.starts_with("*.")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CanonicalDomain {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CanonicalDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for CanonicalDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CanonicalDomain([REDACTED])")
    }
}

impl FromStr for CanonicalDomain {
    type Err = HostsParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum HostsRecord {
    Address(IpAddr),
    Cname(CanonicalDomain),
}

impl fmt::Debug for HostsRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address(_) => formatter.write_str("Address([REDACTED])"),
            Self::Cname(_) => formatter.write_str("Cname([REDACTED])"),
        }
    }
}

impl HostsRecord {
    fn same_as(&self, other: &Self) -> bool {
        self == other
    }

    fn is_cname(&self) -> bool {
        matches!(self, Self::Cname(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostsLimits {
    pub max_input_bytes: usize,
    pub max_records: usize,
    pub max_names: usize,
    pub max_line_bytes: usize,
}

impl Default for HostsLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_records: DEFAULT_MAX_RECORDS,
            max_names: DEFAULT_MAX_NAMES,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum HostsParseError {
    #[error("hosts resource exceeds the input size limit")]
    InputTooLarge,
    #[error("hosts resource exceeds the record limit")]
    TooManyRecords,
    #[error("hosts resource exceeds the owner name limit")]
    TooManyNames,
    #[error("hosts line {line} exceeds the line size limit")]
    LineTooLong { line: usize },
    #[error("hosts line {line} is missing an address")]
    MissingAddress { line: usize },
    #[error("hosts line {line} contains an invalid address")]
    InvalidAddress { line: usize },
    #[error("hosts line {line} is missing a name")]
    MissingName { line: usize },
    #[error("hosts line {line} contains an invalid name")]
    InvalidNameAtLine { line: usize },
    #[error("hosts name is invalid")]
    InvalidName,
    #[error("hosts JSON is invalid")]
    InvalidJson,
    #[error("hosts JSON contains unsupported record type")]
    UnsupportedRecordType,
    #[error("hosts record owner has incompatible record types")]
    ConflictingRecords,
    #[error("hosts record owner contains conflicting CNAME records")]
    ConflictingCname,
}

/// 编译后的 hosts matcher；内部不含 mutable cache，可跨线程共享。
#[derive(Clone, Eq, PartialEq)]
pub struct HostsIndex {
    exact: BTreeMap<CanonicalDomain, Vec<HostsRecord>>,
    wildcard: BTreeMap<CanonicalDomain, Vec<HostsRecord>>,
    record_count: usize,
}

impl fmt::Debug for HostsIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostsIndex")
            .field("exact_owner_count", &self.exact.len())
            .field("wildcard_owner_count", &self.wildcard.len())
            .field("record_count", &self.record_count)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum HostsLookup<'a> {
    Records(&'a [HostsRecord]),
}

impl fmt::Debug for HostsLookup<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Records(records) => formatter
                .debug_struct("Records")
                .field("count", &records.len())
                .finish(),
        }
    }
}

impl HostsIndex {
    pub fn parse_hosts(input: &str) -> Result<Self, HostsParseError> {
        Self::parse_hosts_with_limits(input, HostsLimits::default())
    }

    pub fn parse_hosts_with_limits(
        input: &str,
        limits: HostsLimits,
    ) -> Result<Self, HostsParseError> {
        if input.len() > limits.max_input_bytes {
            return Err(HostsParseError::InputTooLarge);
        }
        let mut index = Self::empty();
        for (line_index, raw_line) in input.lines().enumerate() {
            let line_number = line_index + 1;
            if raw_line.len() > limits.max_line_bytes {
                return Err(HostsParseError::LineTooLong { line: line_number });
            }
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
                .ok_or(HostsParseError::MissingAddress { line: line_number })?
                .parse::<IpAddr>()
                .map_err(|_| HostsParseError::InvalidAddress { line: line_number })?;
            let mut name_count = 0;
            for name in fields {
                name_count += 1;
                let owner = CanonicalDomain::parse(name)
                    .map_err(|_| HostsParseError::InvalidNameAtLine { line: line_number })?;
                index.insert(owner, HostsRecord::Address(address), limits)?;
            }
            if name_count == 0 {
                return Err(HostsParseError::MissingName { line: line_number });
            }
        }
        Ok(index)
    }

    pub fn parse_json(input: &str) -> Result<Self, HostsParseError> {
        Self::parse_json_with_limits(input, HostsLimits::default())
    }

    pub fn parse_json_with_limits(
        input: &str,
        limits: HostsLimits,
    ) -> Result<Self, HostsParseError> {
        if input.len() > limits.max_input_bytes {
            return Err(HostsParseError::InputTooLarge);
        }
        let parsed: BTreeMap<String, JsonHostEntry> =
            yaml_serde::from_str(input).map_err(|_| HostsParseError::InvalidJson)?;
        let mut index = Self::empty();
        for (name, entry) in parsed {
            let (enabled, records) = match entry {
                JsonHostEntry::Records(records) => (true, records),
                JsonHostEntry::Config(config) => (config.enable, config.records),
            };
            if !enabled {
                continue;
            }
            let owner = CanonicalDomain::parse(&name)?;
            for (record_type, values) in records {
                let record_type = record_type.to_ascii_uppercase();
                let values = match values {
                    JsonAddresses::Single(value) => vec![value],
                    JsonAddresses::Multiple(values) => values,
                };
                for value in values {
                    let record = match record_type.as_str() {
                        "A" | "AAAA" => {
                            let address = value
                                .parse::<IpAddr>()
                                .map_err(|_| HostsParseError::InvalidJson)?;
                            if (record_type == "A" && !address.is_ipv4())
                                || (record_type == "AAAA" && !address.is_ipv6())
                            {
                                return Err(HostsParseError::InvalidJson);
                            }
                            HostsRecord::Address(address)
                        }
                        "CNAME" => HostsRecord::Cname(CanonicalDomain::parse(&value)?),
                        _ => return Err(HostsParseError::UnsupportedRecordType),
                    };
                    index.insert(owner.clone(), record, limits)?;
                }
            }
        }
        Ok(index)
    }

    pub fn is_empty(&self) -> bool {
        self.record_count == 0
    }

    pub fn owner_count(&self) -> usize {
        self.exact.len() + self.wildcard.len()
    }

    pub fn record_count(&self) -> usize {
        self.record_count
    }

    pub fn lookup(&self, name: &CanonicalDomain) -> Option<HostsLookup<'_>> {
        if let Some(records) = self.exact.get(name) {
            return Some(HostsLookup::Records(records));
        }
        if name.is_wildcard() {
            return None;
        }
        let labels = name.as_str().split('.').collect::<Vec<_>>();
        for suffix_start in 1..labels.len() {
            let suffix = labels[suffix_start..].join(".");
            let wildcard = CanonicalDomain(format!("*.{suffix}"));
            if let Some(records) = self.wildcard.get(&wildcard) {
                return Some(HostsLookup::Records(records));
            }
        }
        None
    }

    pub fn records(&self, owner: &CanonicalDomain) -> Option<&[HostsRecord]> {
        let map = if owner.is_wildcard() {
            &self.wildcard
        } else {
            &self.exact
        };
        map.get(owner).map(Vec::as_slice)
    }

    fn empty() -> Self {
        Self {
            exact: BTreeMap::new(),
            wildcard: BTreeMap::new(),
            record_count: 0,
        }
    }

    fn insert(
        &mut self,
        owner: CanonicalDomain,
        record: HostsRecord,
        limits: HostsLimits,
    ) -> Result<(), HostsParseError> {
        let owner_count = self.owner_count();
        let map = if owner.is_wildcard() {
            &mut self.wildcard
        } else {
            &mut self.exact
        };
        let is_new_owner = !map.contains_key(&owner);
        if is_new_owner && owner_count >= limits.max_names {
            return Err(HostsParseError::TooManyNames);
        }
        let records = map.entry(owner).or_default();
        if records
            .iter()
            .any(|existing| existing.is_cname() != record.is_cname())
        {
            return Err(HostsParseError::ConflictingRecords);
        }
        if record.is_cname()
            && records
                .iter()
                .any(|existing| existing.is_cname() && !existing.same_as(&record))
        {
            return Err(HostsParseError::ConflictingCname);
        }
        if records.iter().any(|existing| existing.same_as(&record)) {
            return Ok(());
        }
        if self.record_count >= limits.max_records {
            return Err(HostsParseError::TooManyRecords);
        }
        records.push(record);
        self.record_count += 1;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JsonHostEntry {
    Records(BTreeMap<String, JsonAddresses>),
    Config(JsonHostConfig),
}

#[derive(Deserialize)]
struct JsonHostConfig {
    enable: bool,
    #[serde(flatten)]
    records: BTreeMap<String, JsonAddresses>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JsonAddresses {
    Single(String),
    Multiple(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalDomain, HostsIndex, HostsLimits, HostsLookup, HostsParseError, HostsRecord,
    };
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn domain(value: &str) -> CanonicalDomain {
        CanonicalDomain::parse(value).unwrap()
    }

    #[test]
    fn canonicalizes_case_root_dot_and_wildcards() {
        assert_eq!(domain("Example.COM.").as_str(), "example.com");
        assert_eq!(domain("*.Example.COM.").as_str(), "*.example.com");
        assert!(CanonicalDomain::parse("*").is_err());
        assert!(CanonicalDomain::parse("*.example.*.com").is_err());
    }

    #[test]
    fn parses_hosts_and_deduplicates_records() {
        let index = HostsIndex::parse_hosts(
            "# comment\n192.0.2.1 Example.COM alias.example\n192.0.2.1 example.com.\n2001:db8::1 example.com\n",
        )
        .unwrap();
        assert_eq!(index.owner_count(), 2);
        assert_eq!(index.record_count(), 3);
        let records = match index.lookup(&domain("EXAMPLE.com")) {
            Some(HostsLookup::Records(records)) => records,
            None => panic!("expected exact hosts match"),
        };
        assert!(
            records.contains(&HostsRecord::Address(IpAddr::V4(Ipv4Addr::new(
                192, 0, 2, 1
            ))))
        );
        assert!(
            records.contains(&HostsRecord::Address(IpAddr::V6(Ipv6Addr::from(
                0x20010db8000000000000000000000001u128
            ))))
        );
    }

    #[test]
    fn wildcard_is_longest_match_and_does_not_match_apex() {
        let index = HostsIndex::parse_json(
            r#"{
                "*.example.com": {"A": "192.0.2.1"},
                "*.sub.example.com": {"A": "192.0.2.2"},
                "example.com": {"A": "192.0.2.3"}
            }"#,
        )
        .unwrap();
        assert!(index.lookup(&domain("example.com")).is_some());
        assert!(index.lookup(&domain("a.example.com")).is_some_and(
            |HostsLookup::Records(records)| records.contains(&HostsRecord::Address(IpAddr::V4(
                Ipv4Addr::new(192, 0, 2, 1)
            )))
        ));
        assert!(index.lookup(&domain("a.sub.example.com")).is_some_and(
            |HostsLookup::Records(records)| records.contains(&HostsRecord::Address(IpAddr::V4(
                Ipv4Addr::new(192, 0, 2, 2)
            )))
        ));
        assert!(index.lookup(&domain("other.test")).is_none());
    }

    #[test]
    fn parses_cname_and_respects_explicit_disable() {
        let index = HostsIndex::parse_json(
            r#"{
                "alias.example": {"CNAME": "Target.Example."},
                "disabled.example": {"enable": false, "A": "192.0.2.9"}
            }"#,
        )
        .unwrap();
        assert_eq!(index.record_count(), 1);
        let records = index.records(&domain("alias.example")).unwrap();
        assert_eq!(records, &[HostsRecord::Cname(domain("target.example"))]);
        assert!(index.records(&domain("disabled.example")).is_none());
    }

    #[test]
    fn rejects_conflicts_and_unsupported_json_type() {
        assert_eq!(
            HostsIndex::parse_json(r#"{"example.test":{"A":"192.0.2.1","CNAME":"alias.test"}}"#),
            Err(HostsParseError::ConflictingRecords)
        );
        assert_eq!(
            HostsIndex::parse_json(r#"{"example.test":{"CNAME":["a.test","b.test"]}}"#),
            Err(HostsParseError::ConflictingCname)
        );
        assert_eq!(
            HostsIndex::parse_json(r#"{"example.test":{"MX":"mail.test"}}"#),
            Err(HostsParseError::UnsupportedRecordType)
        );
    }

    #[test]
    fn rejects_line_and_size_limits_with_line_numbers() {
        assert_eq!(
            HostsIndex::parse_hosts("\n\n192.0.2.1\n"),
            Err(HostsParseError::MissingName { line: 3 })
        );
        assert_eq!(
            HostsIndex::parse_hosts_with_limits(
                "192.0.2.1 example.test\n",
                HostsLimits {
                    max_line_bytes: 4,
                    ..HostsLimits::default()
                },
            ),
            Err(HostsParseError::LineTooLong { line: 1 })
        );
        assert_eq!(
            HostsIndex::parse_json_with_limits(
                "{}",
                HostsLimits {
                    max_input_bytes: 1,
                    ..HostsLimits::default()
                },
            ),
            Err(HostsParseError::InputTooLarge)
        );
    }

    #[test]
    fn rejects_record_and_name_limits() {
        assert_eq!(
            HostsIndex::parse_hosts_with_limits(
                "192.0.2.1 one.test\n192.0.2.2 two.test\n",
                HostsLimits {
                    max_records: 1,
                    ..HostsLimits::default()
                },
            ),
            Err(HostsParseError::TooManyRecords)
        );
        assert_eq!(
            HostsIndex::parse_hosts_with_limits(
                "192.0.2.1 one.test\n192.0.2.2 two.test\n",
                HostsLimits {
                    max_names: 1,
                    ..HostsLimits::default()
                },
            ),
            Err(HostsParseError::TooManyNames)
        );
    }

    #[test]
    fn debug_redacts_domains_and_addresses() {
        let index = HostsIndex::parse_hosts("192.0.2.1 private.example\n").unwrap();
        let debug = format!("{index:?}");
        assert!(!debug.contains("private.example"));
        assert!(!debug.contains("192.0.2.1"));
        assert!(debug.contains("record_count: 1"));
    }
}
