//! Hosts 资源的有界 const/file loader。

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::config::migrate::deterministic_hash;
use crate::config::model::HostsFormat;
use crate::config::resolve::{ConfigId, ResolvedHostsResource, ResolvedRuleSet};

use super::{HostsIndex, HostsLimits, HostsParseError, RuleIndex, RuleLimits, RuleParseError};

const MAX_STABLE_READ_ATTEMPTS: usize = 2;
const HOSTS_PARSER_VERSION: &str = "hosts-index-v1";
const RULE_PARSER_VERSION: &str = "rule-index-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileFingerprint {
    byte_len: u64,
    modified_unix_nanos: Option<u128>,
}

impl FileFingerprint {
    pub const fn byte_len(self) -> u64 {
        self.byte_len
    }

    pub const fn modified_unix_nanos(self) -> Option<u128> {
        self.modified_unix_nanos
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceSource {
    Const,
    File {
        path: PathBuf,
        fingerprint: FileFingerprint,
    },
}

#[derive(Clone, Debug)]
pub struct LoadedHostsResource {
    id: ConfigId,
    source: ResourceSource,
    content_hash: String,
    fetched_at: SystemTime,
    index: HostsIndex,
}

#[derive(Clone, Debug)]
pub struct LoadedRuleSetResource {
    id: ConfigId,
    source: ResourceSource,
    content_hash: String,
    fetched_at: SystemTime,
    index: RuleIndex,
}

impl LoadedRuleSetResource {
    pub fn id(&self) -> &ConfigId {
        &self.id
    }

    pub fn source(&self) -> &ResourceSource {
        &self.source
    }

    pub fn index(&self) -> &RuleIndex {
        &self.index
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub const fn fetched_at(&self) -> SystemTime {
        self.fetched_at
    }

    pub const fn parser_version(&self) -> &'static str {
        RULE_PARSER_VERSION
    }
}

impl LoadedHostsResource {
    pub fn id(&self) -> &ConfigId {
        &self.id
    }

    pub fn source(&self) -> &ResourceSource {
        &self.source
    }

    pub fn index(&self) -> &HostsIndex {
        &self.index
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub const fn fetched_at(&self) -> SystemTime {
        self.fetched_at
    }

    pub const fn parser_version(&self) -> &'static str {
        HOSTS_PARSER_VERSION
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ResourceLoadError {
    #[error("resource `{resource}` uses unsupported hosts format")]
    UnsupportedFormat { resource: String },
    #[error("resource `{resource}` file path is a symlink")]
    Symlink { resource: String },
    #[error("resource `{resource}` file could not be inspected")]
    Metadata { resource: String },
    #[error("resource `{resource}` file could not be read")]
    Read { resource: String },
    #[error("resource `{resource}` file changed while it was being read")]
    UnstableFile { resource: String },
    #[error("resource `{resource}` is not valid UTF-8")]
    InvalidUtf8 { resource: String },
    #[error("resource `{resource}` exceeds the configured size limit")]
    TooLarge { resource: String },
    #[error("resource `{resource}` could not be parsed: {source}")]
    Parse {
        resource: String,
        #[source]
        source: HostsParseError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum RuleResourceLoadError {
    #[error("rule resource `{resource}` uses unsupported format")]
    UnsupportedFormat { resource: String },
    #[error("rule resource `{resource}` source `{kind}` is not supported")]
    UnsupportedSource {
        resource: String,
        kind: &'static str,
    },
    #[error("rule resource `{resource}` file path is a symlink")]
    Symlink { resource: String },
    #[error("rule resource `{resource}` file could not be inspected")]
    Metadata { resource: String },
    #[error("rule resource `{resource}` file could not be read")]
    Read { resource: String },
    #[error("rule resource `{resource}` file changed while it was being read")]
    UnstableFile { resource: String },
    #[error("rule resource `{resource}` is not valid UTF-8")]
    InvalidUtf8 { resource: String },
    #[error("rule resource `{resource}` exceeds the configured size limit")]
    TooLarge { resource: String },
    #[error("rule resource `{resource}` could not be parsed: {source}")]
    Parse {
        resource: String,
        #[source]
        source: RuleParseError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StableReadError {
    Symlink,
    Metadata,
    Read,
    UnstableFile,
    TooLarge,
}

pub fn load_hosts(
    resource: &ResolvedHostsResource,
    limits: HostsLimits,
) -> Result<LoadedHostsResource, ResourceLoadError> {
    let (id, format, source, bytes) = match resource {
        ResolvedHostsResource::Const { id, format, hosts } => (
            id,
            *format,
            ResourceSource::Const,
            hosts.as_bytes().to_vec(),
        ),
        ResolvedHostsResource::File {
            id, format, path, ..
        } => {
            let (bytes, fingerprint) = read_stable_file(path, limits.max_input_bytes)
                .map_err(|error| map_hosts_file_error(error, id))?;
            (
                id,
                *format,
                ResourceSource::File {
                    path: path.clone(),
                    fingerprint,
                },
                bytes,
            )
        }
    };
    let content_hash = deterministic_hash(&bytes);
    let fetched_at = SystemTime::now();

    if bytes.len() > limits.max_input_bytes {
        return Err(ResourceLoadError::TooLarge {
            resource: id.as_str().to_owned(),
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| ResourceLoadError::InvalidUtf8 {
        resource: id.as_str().to_owned(),
    })?;
    let index = match format {
        HostsFormat::Hosts => HostsIndex::parse_hosts_with_limits(&text, limits),
        HostsFormat::Json => HostsIndex::parse_json_with_limits(&text, limits),
    }
    .map_err(|source| ResourceLoadError::Parse {
        resource: id.as_str().to_owned(),
        source,
    })?;

    Ok(LoadedHostsResource {
        id: id.clone(),
        source,
        content_hash,
        fetched_at,
        index,
    })
}

pub fn load_rule_set(
    resource: &ResolvedRuleSet,
    limits: RuleLimits,
) -> Result<LoadedRuleSetResource, RuleResourceLoadError> {
    let (id, format, source, bytes) = match resource {
        ResolvedRuleSet::Const { id, format, rule } => {
            (id, *format, ResourceSource::Const, rule.as_bytes().to_vec())
        }
        ResolvedRuleSet::File {
            id, format, path, ..
        } => {
            let (bytes, fingerprint) = read_stable_file(path, limits.max_input_bytes)
                .map_err(|error| map_rule_file_error(error, id))?;
            (
                id,
                *format,
                ResourceSource::File {
                    path: path.clone(),
                    fingerprint,
                },
                bytes,
            )
        }
        ResolvedRuleSet::Remote { id, .. } => {
            return Err(RuleResourceLoadError::UnsupportedSource {
                resource: id.as_str().to_owned(),
                kind: "remote",
            });
        }
    };
    let content_hash = deterministic_hash(&bytes);
    let fetched_at = SystemTime::now();

    if bytes.len() > limits.max_input_bytes {
        return Err(RuleResourceLoadError::TooLarge {
            resource: id.as_str().to_owned(),
        });
    }
    if format == crate::config::model::RuleSetFormat::Dat {
        return Err(RuleResourceLoadError::UnsupportedFormat {
            resource: id.as_str().to_owned(),
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| RuleResourceLoadError::InvalidUtf8 {
        resource: id.as_str().to_owned(),
    })?;
    let index = RuleIndex::parse_with_limits(&text, format, limits).map_err(|source| {
        RuleResourceLoadError::Parse {
            resource: id.as_str().to_owned(),
            source,
        }
    })?;

    Ok(LoadedRuleSetResource {
        id: id.clone(),
        source,
        content_hash,
        fetched_at,
        index,
    })
}

fn read_stable_file(
    path: &Path,
    max_bytes: usize,
) -> Result<(Vec<u8>, FileFingerprint), StableReadError> {
    for _ in 0..MAX_STABLE_READ_ATTEMPTS {
        let before = inspect_file(path)?;
        let file = File::open(path).map_err(|_| StableReadError::Read)?;
        let mut bytes = Vec::new();
        file.take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| StableReadError::Read)?;
        if bytes.len() > max_bytes {
            return Err(StableReadError::TooLarge);
        }
        let after = inspect_file(path)?;
        if before == after && before.byte_len == bytes.len() as u64 {
            return Ok((bytes, before));
        }
    }
    Err(StableReadError::UnstableFile)
}

fn inspect_file(path: &Path) -> Result<FileFingerprint, StableReadError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| StableReadError::Metadata)?;
    if metadata.file_type().is_symlink() {
        return Err(StableReadError::Symlink);
    }
    if !metadata.is_file() {
        return Err(StableReadError::Metadata);
    }
    Ok(FileFingerprint {
        byte_len: metadata.len(),
        modified_unix_nanos: metadata.modified().ok().and_then(unix_nanos),
    })
}

fn map_hosts_file_error(error: StableReadError, id: &ConfigId) -> ResourceLoadError {
    let resource = id.as_str().to_owned();
    match error {
        StableReadError::Symlink => ResourceLoadError::Symlink { resource },
        StableReadError::Metadata => ResourceLoadError::Metadata { resource },
        StableReadError::Read => ResourceLoadError::Read { resource },
        StableReadError::UnstableFile => ResourceLoadError::UnstableFile { resource },
        StableReadError::TooLarge => ResourceLoadError::TooLarge { resource },
    }
}

fn map_rule_file_error(error: StableReadError, id: &ConfigId) -> RuleResourceLoadError {
    let resource = id.as_str().to_owned();
    match error {
        StableReadError::Symlink => RuleResourceLoadError::Symlink { resource },
        StableReadError::Metadata => RuleResourceLoadError::Metadata { resource },
        StableReadError::Read => RuleResourceLoadError::Read { resource },
        StableReadError::UnstableFile => RuleResourceLoadError::UnstableFile { resource },
        StableReadError::TooLarge => RuleResourceLoadError::TooLarge { resource },
    }
}

fn unix_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::config::model::{HostsFormat, RuleSetFormat};
    use crate::config::resolve::{ConfigId, ResolvedHostsResource, ResolvedRuleSet};

    use super::{
        FileFingerprint, ResourceLoadError, ResourceSource, RuleResourceLoadError, load_hosts,
        load_rule_set,
    };
    use crate::resource::HostsLimits;

    fn id(value: &str) -> ConfigId {
        ConfigId::new(value).unwrap()
    }

    #[test]
    fn loads_const_hosts_and_json_with_redacted_index() {
        let hosts = ResolvedHostsResource::Const {
            id: id("local-hosts"),
            format: HostsFormat::Hosts,
            hosts: "192.0.2.1 example.test\n".to_owned(),
        };
        let loaded = load_hosts(&hosts, HostsLimits::default()).unwrap();
        assert_eq!(loaded.id().as_str(), "local-hosts");
        assert_eq!(loaded.source(), &ResourceSource::Const);
        assert_eq!(loaded.index().record_count(), 1);

        let json = ResolvedHostsResource::Const {
            id: id("json-hosts"),
            format: HostsFormat::Json,
            hosts: r#"{"example.test":{"A":"192.0.2.2"}}"#.to_owned(),
        };
        let loaded = load_hosts(&json, HostsLimits::default()).unwrap();
        assert_eq!(loaded.index().record_count(), 1);
        let debug = format!("{:?}", loaded.index());
        assert!(!debug.contains("example.test"));
        assert!(!debug.contains("192.0.2.2"));
    }

    #[test]
    fn loads_a_stable_file_and_exposes_only_safe_fingerprint() {
        let path = std::env::temp_dir().join(format!(
            "fluxdns-resource-loader-{}-{}.hosts",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .chars()
                .map(|character| if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '_'
                })
                .collect::<String>()
        ));
        fs::write(&path, "192.0.2.3 file.example\n").unwrap();
        let resource = ResolvedHostsResource::File {
            id: id("file-hosts"),
            format: HostsFormat::Hosts,
            path: path.clone(),
            auto_update: false,
            update_interval: None,
        };
        let loaded = load_hosts(&resource, HostsLimits::default()).unwrap();
        let ResourceSource::File {
            path: loaded_path,
            fingerprint,
        } = loaded.source()
        else {
            panic!("expected file source");
        };
        assert_eq!(loaded_path, &path);
        assert_eq!(fingerprint.byte_len(), 23);
        assert!(fingerprint.modified_unix_nanos().is_some());
        assert_eq!(loaded.index().record_count(), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_oversize_and_invalid_utf8_before_parser() {
        let resource = ResolvedHostsResource::Const {
            id: id("bounded"),
            format: HostsFormat::Hosts,
            hosts: "192.0.2.1 example.test\n".to_owned(),
        };
        assert!(matches!(
            load_hosts(
                &resource,
                HostsLimits {
                    max_input_bytes: 1,
                    ..HostsLimits::default()
                }
            ),
            Err(ResourceLoadError::TooLarge { resource }) if resource == "bounded"
        ));

        let path = std::env::temp_dir().join(format!(
            "fluxdns-resource-loader-invalid-{}.hosts",
            std::process::id()
        ));
        fs::write(&path, [0xff, 0xfe]).unwrap();
        let resource = ResolvedHostsResource::File {
            id: id("invalid-utf8"),
            format: HostsFormat::Hosts,
            path: path.clone(),
            auto_update: false,
            update_interval: None,
        };
        assert!(matches!(
            load_hosts(&resource, HostsLimits::default()),
            Err(ResourceLoadError::InvalidUtf8 { resource }) if resource == "invalid-utf8"
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fingerprint_is_value_comparable() {
        assert_eq!(
            FileFingerprint {
                byte_len: 4,
                modified_unix_nanos: Some(7),
            },
            FileFingerprint {
                byte_len: 4,
                modified_unix_nanos: Some(7),
            }
        );
    }

    #[test]
    fn loads_const_and_file_rule_sets_with_shared_file_boundaries() {
        let inline = ResolvedRuleSet::Const {
            id: id("inline-rules"),
            format: RuleSetFormat::Json,
            rule: r#"{"domain":"example.test"}"#.to_owned(),
        };
        let loaded = load_rule_set(&inline, super::RuleLimits::default()).unwrap();
        assert_eq!(loaded.id().as_str(), "inline-rules");
        assert_eq!(loaded.source(), &ResourceSource::Const);
        assert_eq!(loaded.index().rule_count(), 1);
        assert!(!format!("{:?}", loaded.index()).contains("example.test"));

        let path = std::env::temp_dir().join(format!(
            "fluxdns-rule-loader-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "DOMAIN-SUFFIX,example.test\n").unwrap();
        let file = ResolvedRuleSet::File {
            id: id("file-rules"),
            format: RuleSetFormat::Clash,
            path: path.clone(),
            auto_update: false,
            update_interval: None,
        };
        let loaded = load_rule_set(&file, super::RuleLimits::default()).unwrap();
        assert_eq!(loaded.index().suffix_count(), 1);
        assert!(matches!(loaded.source(), ResourceSource::File { .. }));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_rule_formats_and_sources_not_supported_by_this_slice() {
        let dat = ResolvedRuleSet::Const {
            id: id("dat-rules"),
            format: RuleSetFormat::Dat,
            rule: "opaque".to_owned(),
        };
        assert!(matches!(
            load_rule_set(&dat, super::RuleLimits::default()),
            Err(RuleResourceLoadError::UnsupportedFormat { resource })
                if resource == "dat-rules"
        ));

        let remote = ResolvedRuleSet::Remote {
            id: id("remote-rules"),
            format: RuleSetFormat::Json,
            url: url::Url::parse("https://example.test/rules.json").unwrap(),
            proxy: None,
            auto_update: false,
            update_interval: None,
        };
        assert!(matches!(
            load_rule_set(&remote, super::RuleLimits::default()),
            Err(RuleResourceLoadError::UnsupportedSource { resource, kind })
                if resource == "remote-rules" && kind == "remote"
        ));
    }
}
