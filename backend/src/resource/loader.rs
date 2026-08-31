//! Hosts 资源的有界 const/file loader。

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::config::model::HostsFormat;
use crate::config::resolve::{ConfigId, ResolvedHostsResource};

use super::{HostsIndex, HostsLimits, HostsParseError};

const MAX_STABLE_READ_ATTEMPTS: usize = 2;

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
    index: HostsIndex,
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
}

#[derive(Debug, Error)]
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
            let (bytes, fingerprint) = read_stable_file(path, limits.max_input_bytes, id)?;
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
        index,
    })
}

fn read_stable_file(
    path: &Path,
    max_bytes: usize,
    id: &ConfigId,
) -> Result<(Vec<u8>, FileFingerprint), ResourceLoadError> {
    let resource = id.as_str().to_owned();
    for _ in 0..MAX_STABLE_READ_ATTEMPTS {
        let before = inspect_file(path, &resource)?;
        let file = File::open(path).map_err(|_| ResourceLoadError::Read {
            resource: resource.clone(),
        })?;
        let mut bytes = Vec::new();
        file.take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| ResourceLoadError::Read {
                resource: resource.clone(),
            })?;
        if bytes.len() > max_bytes {
            return Err(ResourceLoadError::TooLarge { resource });
        }
        let after = inspect_file(path, id.as_str())?;
        if before == after && before.byte_len == bytes.len() as u64 {
            return Ok((bytes, before));
        }
    }
    Err(ResourceLoadError::UnstableFile { resource })
}

fn inspect_file(path: &Path, resource: &str) -> Result<FileFingerprint, ResourceLoadError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ResourceLoadError::Metadata {
        resource: resource.to_owned(),
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ResourceLoadError::Symlink {
            resource: resource.to_owned(),
        });
    }
    if !metadata.is_file() {
        return Err(ResourceLoadError::Metadata {
            resource: resource.to_owned(),
        });
    }
    Ok(FileFingerprint {
        byte_len: metadata.len(),
        modified_unix_nanos: metadata.modified().ok().and_then(unix_nanos),
    })
}

fn unix_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::config::model::HostsFormat;
    use crate::config::resolve::{ConfigId, ResolvedHostsResource};

    use super::{FileFingerprint, ResourceLoadError, ResourceSource, load_hosts};
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
            std::thread::current().name().unwrap_or("test")
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
}
