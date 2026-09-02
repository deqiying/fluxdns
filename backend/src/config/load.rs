//! Bounded, version-aware configuration loading.
//!
//! The loader parses and migrates the strict DTO, builds an immutable
//! `ResolvedConfig`, and optionally creates a work-directory snapshot. Secret
//! values are not read during ordinary YAML loading.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, de::DeserializeOwned};
use thiserror::Error;

use super::{
    migrate::{self, MigrationError, MigrationRegistry, MigrationReport},
    model::RawConfig,
    resolve::{self, ResolvedConfig},
    validate::ConfigErrorReport,
};

pub const DEFAULT_MAX_CONFIG_BYTES: usize = 8 * 1024 * 1024;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadOptions {
    pub max_bytes: usize,
    pub create_snapshot: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_CONFIG_BYTES,
            create_snapshot: true,
        }
    }
}

impl LoadOptions {
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }
    pub fn without_snapshot(mut self) -> Self {
        self.create_snapshot = false;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotStatus {
    Skipped,
    SourceInWorkDirectory { path: PathBuf },
    Unchanged { path: PathBuf },
    Created { path: PathBuf },
}

#[derive(Clone, Debug)]
pub struct ConfigLoadOutput {
    pub config: RawConfig,
    pub resolved: std::sync::Arc<ResolvedConfig>,
    pub source_path: Option<PathBuf>,
    pub source_version: u32,
    pub migration_report: MigrationReport,
    pub snapshot: SnapshotStatus,
}

pub struct ConfigLoader {
    options: LoadOptions,
    registry: MigrationRegistry,
}

impl ConfigLoader {
    pub fn new(options: LoadOptions) -> Self {
        Self {
            options,
            registry: migrate::current_registry(),
        }
    }

    pub fn with_registry(options: LoadOptions, registry: MigrationRegistry) -> Self {
        Self { options, registry }
    }

    pub fn options(&self) -> LoadOptions {
        self.options
    }
    pub fn registry(&self) -> &MigrationRegistry {
        &self.registry
    }

    pub fn load_from_path<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<ConfigLoadOutput, ConfigLoadError> {
        self.load_path(path)
    }

    pub fn load_from_bytes(&self, bytes: &[u8]) -> Result<ConfigLoadOutput, ConfigLoadError> {
        self.load_bytes(bytes)
    }

    pub fn load_from_str(&self, source: &str) -> Result<ConfigLoadOutput, ConfigLoadError> {
        self.load_str(source)
    }

    pub fn load_path<P: AsRef<Path>>(&self, path: P) -> Result<ConfigLoadOutput, ConfigLoadError> {
        let source_path = absolute_config_path(path.as_ref())?;
        let bytes = read_bounded_file(&source_path, self.options.max_bytes)?;
        let mut output = self.load_bytes_inner(&bytes, Some(source_path.clone()))?;
        if self.options.create_snapshot {
            output.snapshot = create_snapshot(
                &output.resolved.work.path,
                &source_path,
                &bytes,
                self.options.max_bytes,
            )?;
        }
        Ok(output)
    }

    pub fn load_bytes(&self, bytes: &[u8]) -> Result<ConfigLoadOutput, ConfigLoadError> {
        let bytes = bounded_bytes(bytes, self.options.max_bytes)?;
        let mut output = self.load_bytes_inner(bytes, None)?;
        output.snapshot = SnapshotStatus::Skipped;
        Ok(output)
    }

    pub fn load_str(&self, source: &str) -> Result<ConfigLoadOutput, ConfigLoadError> {
        self.load_bytes(source.as_bytes())
    }

    fn load_bytes_inner(
        &self,
        bytes: &[u8],
        source_path: Option<PathBuf>,
    ) -> Result<ConfigLoadOutput, ConfigLoadError> {
        let header: VersionHeader = deserialize_yaml(bytes, ParseStage::VersionHeader)?;
        let migration = self
            .registry
            .migrate(header.version, bytes)
            .map_err(ConfigLoadError::Migration)?;
        if migration.document.len() > self.options.max_bytes {
            return Err(ConfigLoadError::TooLarge {
                limit: self.options.max_bytes,
            });
        }
        let config: RawConfig = deserialize_yaml(&migration.document, ParseStage::Config)?;
        if config.version != self.registry.current_version() {
            return Err(ConfigLoadError::VersionMismatch {
                expected: self.registry.current_version(),
                actual: config.version,
            });
        }
        let config_dir = source_path.as_deref().and_then(Path::parent);
        let resolved = resolve::resolve_config_with_base_dir(
            &config,
            migration.report.input_hash.clone(),
            config_dir,
        )
        .map_err(ConfigLoadError::Validation)?
        .resolved;
        Ok(ConfigLoadOutput {
            config,
            resolved,
            source_path,
            source_version: header.version,
            migration_report: migration.report,
            snapshot: SnapshotStatus::Skipped,
        })
    }
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new(LoadOptions::default())
    }
}

pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<ConfigLoadOutput, ConfigLoadError> {
    ConfigLoader::default().load_path(path)
}

pub fn load_from_bytes(bytes: &[u8]) -> Result<ConfigLoadOutput, ConfigLoadError> {
    ConfigLoader::default().load_bytes(bytes)
}

pub fn load_from_str(source: &str) -> Result<ConfigLoadOutput, ConfigLoadError> {
    ConfigLoader::default().load_str(source)
}

#[derive(Clone, Copy, Debug)]
enum ParseStage {
    VersionHeader,
    Config,
}

impl ParseStage {
    fn message(self) -> &'static str {
        match self {
            Self::VersionHeader => "invalid or missing configuration schema version",
            Self::Config => "configuration does not match the strict schema",
        }
    }
}

#[derive(Debug, Deserialize)]
struct VersionHeader {
    version: u32,
}

#[derive(Debug, Error)]
pub enum ConfigLoadError {
    #[error("configuration is empty")]
    Empty,
    #[error("configuration exceeds the {limit} byte limit")]
    TooLarge { limit: usize },
    #[error("configuration is not valid UTF-8 at byte {valid_up_to}")]
    InvalidUtf8 { valid_up_to: usize },
    #[error("{stage}: path={path}{location}", location = format_location(*line, *column))]
    Parse {
        stage: &'static str,
        path: String,
        line: Option<usize>,
        column: Option<usize>,
    },
    #[error("migration failed: {0}")]
    Migration(MigrationError),
    #[error("migrated configuration version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: u32, actual: u32 },
    #[error("configuration validation failed: {0}")]
    Validation(ConfigErrorReport),
    #[error("configuration path must be absolute for a work snapshot: {path}")]
    RelativeWorkPath { path: PathBuf },
    #[error("configuration snapshot differs from existing file: {path}")]
    SnapshotConflict { path: PathBuf },
    #[error("configuration snapshot target must not be a symlink or special file: {path}")]
    SnapshotSymlink { path: PathBuf },
    #[error("configuration snapshot operation {operation} failed for {path}: {source}")]
    SnapshotIo {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("resolving configuration path {path} failed: {source}")]
    ResolvePath {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("reading configuration {path} failed: {source}")]
    ReadIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn format_location(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(line), Some(column)) => format!(" at line {line}, column {column}"),
        (Some(line), None) => format!(" at line {line}"),
        (None, Some(column)) => format!(" at column {column}"),
        (None, None) => String::new(),
    }
}

fn absolute_config_path(path: &Path) -> Result<PathBuf, ConfigLoadError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let current = std::env::current_dir().map_err(|source| ConfigLoadError::ResolvePath {
            path: path.to_path_buf(),
            source,
        })?;
        current.join(path)
    };
    Ok(resolve::lexical_normalize(&absolute))
}

fn bounded_bytes(bytes: &[u8], limit: usize) -> Result<&[u8], ConfigLoadError> {
    if bytes.is_empty() {
        return Err(ConfigLoadError::Empty);
    }
    if bytes.len() > limit {
        return Err(ConfigLoadError::TooLarge { limit });
    }
    std::str::from_utf8(bytes).map_err(|error| ConfigLoadError::InvalidUtf8 {
        valid_up_to: error.valid_up_to(),
    })?;
    Ok(bytes)
}

fn read_bounded_file(path: &Path, limit: usize) -> Result<Vec<u8>, ConfigLoadError> {
    let metadata = fs::metadata(path).map_err(|source| ConfigLoadError::ReadIo {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > limit as u64 {
        return Err(ConfigLoadError::TooLarge { limit });
    }
    let file = File::open(path).map_err(|source| ConfigLoadError::ReadIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(metadata.len().min(limit as u64) as usize);
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigLoadError::ReadIo {
            path: path.to_path_buf(),
            source,
        })?;
    bounded_bytes(&bytes, limit)?;
    Ok(bytes)
}

fn deserialize_yaml<T: DeserializeOwned>(
    bytes: &[u8],
    stage: ParseStage,
) -> Result<T, ConfigLoadError> {
    let deserializer = yaml_serde::Deserializer::from_slice(bytes);
    serde_path_to_error::deserialize(deserializer).map_err(|error| {
        let location = error.inner().location();
        ConfigLoadError::Parse {
            stage: stage.message(),
            path: error.path().to_string(),
            line: location.as_ref().map(yaml_serde::Location::line),
            column: location.as_ref().map(yaml_serde::Location::column),
        }
    })
}

fn create_snapshot(
    work_path: &Path,
    source_path: &Path,
    bytes: &[u8],
    max_bytes: usize,
) -> Result<SnapshotStatus, ConfigLoadError> {
    create_snapshot_with_hook(work_path, source_path, bytes, max_bytes, || {})
}

fn create_snapshot_with_hook<F>(
    work_path: &Path,
    source_path: &Path,
    bytes: &[u8],
    max_bytes: usize,
    before_publish: F,
) -> Result<SnapshotStatus, ConfigLoadError>
where
    F: FnOnce(),
{
    if !work_path.is_absolute() {
        return Err(ConfigLoadError::RelativeWorkPath {
            path: work_path.to_path_buf(),
        });
    }
    validate_work_path_components(work_path)?;
    let target = work_path.join("config.yaml");
    let source_parent = source_path.parent().unwrap_or_else(|| Path::new("."));
    if equivalent_directory(source_parent, work_path) {
        return Ok(SnapshotStatus::SourceInWorkDirectory { path: target });
    }
    fs::create_dir_all(work_path).map_err(|source| ConfigLoadError::SnapshotIo {
        operation: "create work directory",
        path: work_path.to_path_buf(),
        source,
    })?;
    validate_work_path_components(work_path)?;
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ConfigLoadError::SnapshotSymlink { path: target });
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(ConfigLoadError::SnapshotConflict { path: target });
        }
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(ConfigLoadError::SnapshotIo {
                operation: "inspect snapshot target",
                path: target,
                source,
            });
        }
    }
    match read_bounded_file(&target, max_bytes) {
        Ok(existing) if existing == bytes => return Ok(SnapshotStatus::Unchanged { path: target }),
        Ok(_) => return Err(ConfigLoadError::SnapshotConflict { path: target }),
        Err(ConfigLoadError::ReadIo { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
        }
        Err(error) => return Err(error),
    }
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = work_path.join(format!(
        ".config.yaml.fluxdns-{}-{sequence}.tmp",
        std::process::id()
    ));
    if let Err(error) = write_temp_snapshot(&temp, bytes) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    before_publish();
    if let Err(error) = validate_work_path_components(work_path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(source) = fs::hard_link(&temp, &target) {
        let _ = fs::remove_file(&temp);
        if source.kind() == io::ErrorKind::AlreadyExists {
            return match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    Err(ConfigLoadError::SnapshotSymlink { path: target })
                }
                Ok(metadata) if metadata.file_type().is_file() => {
                    match read_bounded_file(&target, max_bytes) {
                        Ok(existing) if existing == bytes => {
                            Ok(SnapshotStatus::Unchanged { path: target })
                        }
                        Ok(_) => Err(ConfigLoadError::SnapshotConflict { path: target }),
                        Err(error) => Err(error),
                    }
                }
                Ok(_) => Err(ConfigLoadError::SnapshotConflict { path: target }),
                Err(source) => Err(ConfigLoadError::SnapshotIo {
                    operation: "inspect concurrently-created snapshot",
                    path: target,
                    source,
                }),
            };
        }
        return Err(ConfigLoadError::SnapshotIo {
            operation: "atomically publish snapshot",
            path: target,
            source,
        });
    }
    let _ = fs::remove_file(&temp);
    sync_directory(work_path)?;
    Ok(SnapshotStatus::Created {
        path: work_path.join("config.yaml"),
    })
}

/// Reject a symlinked work path or its nearest existing parent before
/// path-based publication. Standard macOS aliases such as `/var` may remain
/// outside this check; callers still need protected parents to eliminate
/// TOCTOU races.
fn validate_work_path_components(path: &Path) -> Result<(), ConfigLoadError> {
    let mut current = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ConfigLoadError::SnapshotSymlink { path: current });
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(ConfigLoadError::SnapshotConflict { path: current });
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let Some(parent) = current.parent() else {
                    return Ok(());
                };
                current = parent.to_path_buf();
                continue;
            }
            Err(source) => {
                return Err(ConfigLoadError::SnapshotIo {
                    operation: "inspect work directory",
                    path: current,
                    source,
                });
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ConfigLoadError::SnapshotSymlink {
                    path: parent.to_path_buf(),
                });
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(ConfigLoadError::SnapshotConflict {
                    path: parent.to_path_buf(),
                });
            }
            Ok(_) => break,
            Err(source) if source.kind() == io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(ConfigLoadError::SnapshotIo {
                    operation: "inspect work directory",
                    path: parent.to_path_buf(),
                    source,
                });
            }
        }
    }
    Ok(())
}

fn write_temp_snapshot(path: &Path, bytes: &[u8]) -> Result<(), ConfigLoadError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|source| ConfigLoadError::SnapshotIo {
            operation: "create temporary snapshot",
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(bytes)
        .map_err(|source| ConfigLoadError::SnapshotIo {
            operation: "write temporary snapshot",
            path: path.to_path_buf(),
            source,
        })?;
    file.flush().map_err(|source| ConfigLoadError::SnapshotIo {
        operation: "flush temporary snapshot",
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all()
        .map_err(|source| ConfigLoadError::SnapshotIo {
            operation: "sync temporary snapshot",
            path: path.to_path_buf(),
            source,
        })
}

fn sync_directory(path: &Path) -> Result<(), ConfigLoadError> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ConfigLoadError::SnapshotIo {
            operation: "sync snapshot directory",
            path: path.to_path_buf(),
            source,
        })?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn equivalent_directory(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| lexical_absolute(left));
    let right = fs::canonicalize(right).unwrap_or_else(|_| lexical_absolute(right));
    left == right
}

fn lexical_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigLoadError, SnapshotStatus, bounded_bytes, create_snapshot, create_snapshot_with_hook,
    };
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEMP_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("fluxdns-load-{suffix}-{sequence}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn bounded_input_rejects_empty_oversize_and_invalid_utf8() {
        assert!(matches!(bounded_bytes(&[], 8), Err(ConfigLoadError::Empty)));
        assert!(matches!(
            bounded_bytes(b"123456789", 8),
            Err(ConfigLoadError::TooLarge { limit: 8 })
        ));
        assert!(matches!(
            bounded_bytes(&[0xff], 8),
            Err(ConfigLoadError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn snapshot_is_idempotent_and_refuses_different_content() {
        let root = temp_dir();
        let source = root.join("input.yaml");
        let work = root.join("work");
        fs::write(&source, b"version: 1\n").unwrap();
        let first = create_snapshot(&work, &source, b"version: 1\n", 128).unwrap();
        assert!(matches!(first, SnapshotStatus::Created { .. }));
        let second = create_snapshot(&work, &source, b"version: 1\n", 128).unwrap();
        assert!(matches!(second, SnapshotStatus::Unchanged { .. }));
        let error = create_snapshot(&work, &source, b"version: 2\n", 128).unwrap_err();
        assert!(matches!(error, ConfigLoadError::SnapshotConflict { .. }));
        assert_eq!(fs::read(work.join("config.yaml")).unwrap(), b"version: 1\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_rejects_symlink_target_without_touching_linked_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let root = temp_dir();
            let source = root.join("input.yaml");
            let work = root.join("work");
            let external = root.join("external.yaml");
            fs::create_dir_all(&work).unwrap();
            fs::write(&source, b"version: 1\n").unwrap();
            fs::write(&external, b"external\n").unwrap();
            symlink(&external, work.join("config.yaml")).unwrap();

            let error = create_snapshot(&work, &source, b"version: 1\n", 128).unwrap_err();
            assert!(matches!(error, ConfigLoadError::SnapshotSymlink { .. }));
            assert_eq!(fs::read(&external).unwrap(), b"external\n");
            assert!(
                fs::symlink_metadata(work.join("config.yaml"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn snapshot_rejects_work_directory_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let root = temp_dir();
            let source = root.join("input.yaml");
            let real_work = root.join("real-work");
            let linked_work = root.join("linked-work");
            fs::create_dir_all(&real_work).unwrap();
            fs::write(&source, b"version: 1\n").unwrap();
            symlink(&real_work, &linked_work).unwrap();

            let error = create_snapshot(&linked_work, &source, b"version: 1\n", 128).unwrap_err();
            assert!(
                matches!(error, ConfigLoadError::SnapshotSymlink { path } if path == linked_work)
            );
            assert!(!real_work.join("config.yaml").exists());
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn snapshot_rejects_static_symlinked_parent_directory() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let root = temp_dir();
            let source = root.join("input.yaml");
            let real_parent = root.join("real-parent");
            let linked_parent = root.join("linked-parent");
            let work = linked_parent.join("work");
            fs::create_dir_all(&real_parent).unwrap();
            fs::write(&source, b"version: 1\n").unwrap();
            symlink(&real_parent, &linked_parent).unwrap();

            let error = create_snapshot(&work, &source, b"version: 1\n", 128).unwrap_err();
            assert!(
                matches!(error, ConfigLoadError::SnapshotSymlink { path } if path == linked_parent)
            );
            assert!(!real_parent.join("work").exists());
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn snapshot_does_not_overwrite_target_appearing_after_initial_check() {
        let root = temp_dir();
        let source = root.join("input.yaml");
        let work = root.join("work");
        let target = work.join("config.yaml");
        fs::write(&source, b"version: 1\n").unwrap();
        let error = create_snapshot_with_hook(&work, &source, b"version: 1\n", 128, || {
            fs::write(&target, b"appeared\n").unwrap();
        })
        .unwrap_err();
        assert!(matches!(error, ConfigLoadError::SnapshotConflict { path } if path == target));
        assert_eq!(fs::read(&target).unwrap(), b"appeared\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_snapshot_publish_has_one_creator_and_no_overwrite() {
        use std::sync::Arc;
        use std::thread;

        let root = temp_dir();
        let source = root.join("input.yaml");
        let work = Arc::new(root.join("work"));
        fs::write(&source, b"version: 1\n").unwrap();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let work = Arc::clone(&work);
            let source = source.clone();
            handles.push(thread::spawn(move || {
                create_snapshot(&work, &source, b"version: 1\n", 128).unwrap()
            }));
        }
        let statuses: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            statuses
                .iter()
                .filter(|status| matches!(status, SnapshotStatus::Created { .. }))
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| matches!(status, SnapshotStatus::Unchanged { .. }))
                .count(),
            7
        );
        assert_eq!(fs::read(work.join("config.yaml")).unwrap(), b"version: 1\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repository_example_is_strictly_loaded_and_resolved_without_external_io() {
        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let (source, work) = crate::config::test_support::portable_example();
        let output = loader
            .load_str(&source)
            .expect("repository configuration example should satisfy the v1 contract");
        assert_eq!(output.config.version, 1);
        assert_eq!(output.resolved.version, 1);
        assert_eq!(output.resolved.listeners.len(), 3);
        assert_eq!(output.resolved.upstreams.len(), 7);
        assert!(!output.resolved.normalized_hash.is_empty());
        assert_eq!(output.resolved.work.rules_path, work.join("rules"));
        let inner = output
            .resolved
            .strategies
            .iter()
            .find(|strategy| strategy.id.as_str() == "inner")
            .unwrap();
        let selector = inner.rules[0].rule_set.as_ref().unwrap();
        assert_eq!(selector.resource.as_str(), "geosite");
        assert_eq!(selector.selector.as_deref(), Some("cn"));

        let doh = output
            .resolved
            .listeners
            .iter()
            .find_map(|listener| match listener {
                super::super::resolve::ResolvedListener::Doh { endpoints, .. } => Some(endpoints),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            doh[1].client_ip.source,
            super::super::model::ClientIpSource::ForwardedHeader
        );
        assert_eq!(doh[1].client_ip.trusted_proxies.as_ref().unwrap().len(), 2);
        assert_eq!(doh[0].binding.listener_id, "doh");
        assert_eq!(doh[0].binding.endpoint_id, doh[0].id.as_str());
    }

    #[test]
    fn strict_loader_rejects_unknown_fields_and_duplicate_documents() {
        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let unknown = format!(
            "{}\nunknown: true\n",
            include_str!("../../../config-example.yaml")
        );
        assert!(matches!(
            loader.load_str(&unknown),
            Err(ConfigLoadError::Parse { .. })
        ));

        let duplicate = format!(
            "{}\nversion: 1\n",
            include_str!("../../../config-example.yaml")
        );
        assert!(matches!(
            loader.load_str(&duplicate),
            Err(ConfigLoadError::Parse { .. })
        ));

        let multiple = format!(
            "{}\n---\nversion: 1\n",
            include_str!("../../../config-example.yaml")
        );
        assert!(matches!(
            loader.load_str(&multiple),
            Err(ConfigLoadError::Parse { .. })
        ));

        for source in ["null", "[]", "scalar"] {
            assert!(matches!(
                loader.load_str(source),
                Err(ConfigLoadError::Parse { .. })
            ));
        }
    }

    #[test]
    fn loader_rejects_future_schema_versions_before_current_dto_validation() {
        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let error = loader.load_str("version: 2\n").unwrap_err();
        assert!(matches!(
            error,
            ConfigLoadError::Migration(super::super::migrate::MigrationError::FutureVersion {
                input: 2,
                current: 1
            })
        ));
    }

    #[test]
    fn strict_loader_rejects_explicit_null_instead_of_treating_it_as_missing() {
        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let source = include_str!("../../../config-example.yaml").replacen(
            "  cache:\n",
            "  cache: null\n",
            1,
        );
        assert!(matches!(
            loader.load_str(&source),
            Err(ConfigLoadError::Parse { .. })
        ));
    }

    #[test]
    fn loader_normalizes_case_insensitive_log_level() {
        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let (source, _) = crate::config::test_support::portable_example();
        let source = source.replace("level: info", "level: INFO");
        let output = loader.load_str(&source).unwrap();
        assert_eq!(
            output.config.logs.level,
            super::super::model::LogLevelDto::Info
        );
    }

    #[test]
    fn loader_rejects_empty_tls_certificate_path() {
        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let source = include_str!("../../../config-example.yaml").replace(
            "certificate_file: ./tls/fullchain.pem",
            "certificate_file: \"\"",
        );
        assert!(matches!(
            loader.load_str(&source),
            Err(ConfigLoadError::Validation(_))
        ));
    }

    #[test]
    fn configuration_debug_is_redacted_and_normalized_hash_tracks_secret_metadata() {
        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let (source, _) = crate::config::test_support::portable_example();
        let output = loader.load_str(&source).unwrap();
        let debug = format!("{:?}", output.config);
        assert!(!debug.contains("jkBqXYJuD.4MlWKN"));
        assert!(!debug.contains("FLUXDNS_OUTBOUND_SG_URL"));
        assert!(!debug.contains("7a753d8a-a5c7-4e37-a207-9b0e15d9009f"));
        assert!(!debug.contains("192.168.1.0/24"));
        assert!(!debug.contains("/dns-query"));

        let resolved_client_debug = format!("{:?}", output.resolved.clients[0]);
        assert!(!resolved_client_debug.contains("7a753d8a-a5c7-4e37-a207-9b0e15d9009f"));
        assert!(!resolved_client_debug.contains("192.168.1.0/24"));

        let changed_source =
            source.replace("FLUXDNS_OUTBOUND_SG_URL", "FLUXDNS_OUTBOUND_OTHER_URL");
        let changed = loader.load_str(&changed_source).unwrap();
        assert_ne!(
            output.resolved.normalized_hash,
            changed.resolved.normalized_hash
        );

        let equivalent_source = source
            .replace("rules_path: ./rules", "rules_path: rules")
            .replace("path: ./data/fluxdns.sqlite3", "path: data/fluxdns.sqlite3")
            .replace("path: ./logs/fluxdns.log", "path: logs/fluxdns.log");
        let equivalent = loader.load_str(&equivalent_source).unwrap();
        assert_eq!(
            output.resolved.normalized_hash,
            equivalent.resolved.normalized_hash
        );
    }

    #[test]
    fn load_path_resolves_relative_work_and_project_paths_from_config_directory() {
        let root = temp_dir();
        let config_dir = root.join("bootstrap");
        let config_path = config_dir.join("config.yaml");
        fs::create_dir_all(&config_dir).unwrap();
        let source = include_str!("../../../config-example.yaml")
            .replace("path: /etc/fluxdns", "path: ../runtime")
            .replace("rules_path: ./rules", "rules_path: ./rules/../rules")
            .replace(
                "path: ./data/fluxdns.sqlite3",
                "path: ./data/./fluxdns.sqlite3",
            );
        fs::write(&config_path, source).unwrap();

        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let output = loader.load_from_path(&config_path).unwrap();
        let work = root.join("runtime");
        assert_eq!(output.resolved.work.path, work);
        assert_eq!(output.resolved.work.rules_path, work.join("rules"));
        assert_eq!(
            output.resolved.database.path,
            work.join("data/fluxdns.sqlite3")
        );
        assert_eq!(output.resolved.logs.path, work.join("logs/fluxdns.log"));
        assert_eq!(
            output.resolved.dns.cache.persistence_path,
            work.join("cache.db")
        );
        assert_eq!(output.source_path, Some(config_path));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_path_uses_resolved_work_path_for_snapshot() {
        let root = temp_dir();
        let config_dir = root.join("bootstrap");
        let config_path = config_dir.join("input.yaml");
        fs::create_dir_all(&config_dir).unwrap();
        let source = include_str!("../../../config-example.yaml")
            .replace("path: /etc/fluxdns", "path: ../runtime");
        fs::write(&config_path, source.as_bytes()).unwrap();

        let output = super::ConfigLoader::default()
            .load_from_path(&config_path)
            .unwrap();
        let snapshot = root.join("runtime/config.yaml");
        assert_eq!(output.resolved.work.snapshot_path, snapshot);
        assert!(matches!(
            output.snapshot,
            SnapshotStatus::Created { ref path } if path == &snapshot
        ));
        assert_eq!(fs::read(&snapshot).unwrap(), source.as_bytes());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_without_source_rejects_relative_work_path() {
        let source =
            include_str!("../../../config-example.yaml").replace("path: /etc/fluxdns", "path: ./");
        let loader = super::ConfigLoader::new(super::LoadOptions::default().without_snapshot());
        let error = loader.load_str(&source).unwrap_err();
        match error {
            ConfigLoadError::Validation(report) => {
                assert_eq!(report.errors.len(), 1);
                assert_eq!(report.errors[0].path, "work.path");
                assert_eq!(
                    report.errors[0].message,
                    "relative work.path requires a configuration file base directory"
                );
            }
            other => panic!("expected missing config base error, got {other:?}"),
        }
    }
}
