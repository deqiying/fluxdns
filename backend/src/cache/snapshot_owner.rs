//! 进程级缓存快照 owner、代际仲裁与生命周期状态。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::task::JoinHandle;

use crate::dns::{Deadline, RuntimeRevision};
use crate::ports::PortErrorClass;
use crate::ports::cache::{CacheCondition, CacheRecoverySummary, CacheStore, CacheWriteOutcome};

use super::moka::MokaCacheStore;
use super::snapshot::{
    CacheSnapshotError, CacheSnapshotWriteSummary, open_cache_snapshot,
    write_cache_snapshot_if_current,
};

const SNAPSHOT_BATCH_SIZE: usize = 128;
const SNAPSHOT_FILE_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;
const SNAPSHOT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CacheSnapshotSettings {
    pub enabled: bool,
    pub path: PathBuf,
    pub interval: Duration,
    pub protected_paths: Vec<PathBuf>,
}

impl CacheSnapshotSettings {
    /// BC-07 过渡接线：v1 只提供路径，周期使用已冻结的 5 分钟默认值。
    /// 旧 `max_size_bytes` 不进入新快照语义；v2 字段由 BC-26 正式装载。
    pub(crate) fn from_current_config(
        config: &crate::config::ResolvedConfig,
        enabled: bool,
    ) -> Result<Self, CacheSnapshotOwnerBuildError> {
        Self::new(
            enabled,
            config.dns.cache.persistence_path.clone(),
            Duration::from_secs(300),
            vec![
                config.database.path.clone(),
                config.logs.path.clone(),
                config.work.snapshot_path.clone(),
            ],
        )
    }

    pub(crate) fn new(
        enabled: bool,
        path: PathBuf,
        interval: Duration,
        protected_paths: Vec<PathBuf>,
    ) -> Result<Self, CacheSnapshotOwnerBuildError> {
        if interval.is_zero() {
            return Err(CacheSnapshotOwnerBuildError::ZeroInterval);
        }
        validate_managed_path(&path, &protected_paths)?;
        Ok(Self {
            enabled,
            path,
            interval,
            protected_paths,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheSnapshotCondition {
    Disabled,
    Idle,
    Writing,
    #[allow(dead_code)] // 启动恢复在 owner 发布前完成；BC-12 状态端点保留该正式状态。
    Restoring,
    Failed,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheSnapshotFailure {
    Timeout,
    Unavailable,
    Corrupt,
    Incompatible,
    ResourceExhausted,
    Io,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CacheSnapshotOwnerStatus {
    pub condition: CacheSnapshotCondition,
    pub owner_revision: RuntimeRevision,
    pub generation: u64,
    pub file_bytes: Option<u64>,
    pub last_success_at_utc_millis: Option<u64>,
    pub last_error: Option<CacheSnapshotFailure>,
    pub recovery: CacheRecoverySummary,
    pub recovery_skipped: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CacheSnapshotShutdownSummary {
    pub completed: bool,
    pub attempted: bool,
    pub written: bool,
    pub error: Option<CacheSnapshotFailure>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RecoveryResult {
    summary: CacheRecoverySummary,
    skipped: u64,
    file_bytes: Option<u64>,
    last_success_at_utc_millis: Option<u64>,
    complete: bool,
    error: Option<CacheSnapshotFailure>,
}

#[derive(Debug, Error)]
pub(crate) enum CacheSnapshotOwnerBuildError {
    #[error("cache snapshot interval must be greater than zero")]
    ZeroInterval,
    #[error("cache snapshot path is not an owned regular-file target")]
    InvalidPath,
    #[error("cache snapshot owner requires a Tokio runtime")]
    MissingRuntime,
    #[error("cache snapshot owner requires a policy cache source")]
    MissingSource,
    #[error("cache snapshot owner source does not match the active runtime")]
    SourceMismatch,
    #[error("cache snapshot owner is already attached")]
    AlreadyAttached,
}

#[derive(Clone)]
struct SnapshotSource {
    owner_revision: RuntimeRevision,
    generation: u64,
    store: Arc<MokaCacheStore>,
    settings: CacheSnapshotSettings,
}

struct OwnerState {
    source: SnapshotSource,
    status: CacheSnapshotOwnerStatus,
    last_persisted_source_generation: Option<u64>,
    stopped: bool,
}

struct OwnerInner {
    state: Mutex<OwnerState>,
    publish: Mutex<()>,
    changed: tokio::sync::Notify,
}

/// 唯一进程级缓存快照 owner；reload 只切换 source，不重新读取磁盘。
pub(crate) struct CacheSnapshotOwner {
    inner: Arc<OwnerInner>,
    task: Mutex<Option<JoinHandle<()>>>,
}

pub(crate) struct PreparedCacheSnapshotSwitch {
    owner_revision: RuntimeRevision,
    store: Arc<MokaCacheStore>,
    settings: CacheSnapshotSettings,
}

impl CacheSnapshotOwner {
    /// 启动时先有界恢复当前 Moka，再启动唯一周期 worker。
    pub(crate) async fn start(
        owner_revision: RuntimeRevision,
        store: Arc<MokaCacheStore>,
        settings: CacheSnapshotSettings,
        deadline: Deadline,
    ) -> Result<Arc<Self>, CacheSnapshotOwnerBuildError> {
        tokio::runtime::Handle::try_current()
            .map_err(|_| CacheSnapshotOwnerBuildError::MissingRuntime)?;
        let recovery = if settings.enabled {
            recover_snapshot(Arc::clone(&store), settings.path.clone(), deadline).await
        } else {
            RecoveryResult {
                complete: true,
                ..RecoveryResult::default()
            }
        };
        let generation = 1;
        let condition = if !settings.enabled {
            CacheSnapshotCondition::Disabled
        } else if recovery.error.is_some() {
            CacheSnapshotCondition::Failed
        } else {
            CacheSnapshotCondition::Idle
        };
        let last_persisted_source_generation = (settings.enabled
            && recovery.complete
            && recovery.error.is_none()
            && recovery.skipped == 0
            && recovery.file_bytes.is_some())
        .then(|| store.snapshot_generation());
        let inner = Arc::new(OwnerInner {
            state: Mutex::new(OwnerState {
                source: SnapshotSource {
                    owner_revision,
                    generation,
                    store,
                    settings,
                },
                status: CacheSnapshotOwnerStatus {
                    condition,
                    owner_revision,
                    generation,
                    file_bytes: recovery.file_bytes,
                    last_success_at_utc_millis: recovery.last_success_at_utc_millis,
                    last_error: recovery.error,
                    recovery: recovery.summary,
                    recovery_skipped: recovery.skipped,
                },
                last_persisted_source_generation,
                stopped: false,
            }),
            publish: Mutex::new(()),
            changed: tokio::sync::Notify::new(),
        });
        let task = tokio::spawn(run_periodic(Arc::downgrade(&inner)));
        Ok(Arc::new(Self {
            inner,
            task: Mutex::new(Some(task)),
        }))
    }

    pub(crate) fn status(&self) -> CacheSnapshotOwnerStatus {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
    }

    pub(crate) fn matches_source(
        &self,
        owner_revision: RuntimeRevision,
        store: &Arc<MokaCacheStore>,
    ) -> bool {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.source.owner_revision == owner_revision && Arc::ptr_eq(&state.source.store, store)
    }

    pub(crate) fn prepare_switch(
        &self,
        owner_revision: RuntimeRevision,
        store: Arc<MokaCacheStore>,
        settings: CacheSnapshotSettings,
    ) -> PreparedCacheSnapshotSwitch {
        PreparedCacheSnapshotSwitch {
            owner_revision,
            store,
            settings,
        }
    }

    /// 此提交只持有短仲裁锁并更新内存状态，不能在 Runtime CAS 后失败。
    pub(crate) fn publish_switch(&self, prepared: PreparedCacheSnapshotSwitch) {
        let _publish = self
            .inner
            .publish
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = state.source.generation.saturating_add(1);
        state.source = SnapshotSource {
            owner_revision: prepared.owner_revision,
            generation,
            store: prepared.store,
            settings: prepared.settings,
        };
        state.status = CacheSnapshotOwnerStatus {
            condition: if state.source.settings.enabled {
                CacheSnapshotCondition::Idle
            } else {
                CacheSnapshotCondition::Disabled
            },
            owner_revision: state.source.owner_revision,
            generation,
            file_bytes: None,
            last_success_at_utc_millis: None,
            last_error: None,
            recovery: CacheRecoverySummary::default(),
            recovery_skipped: 0,
        };
        state.last_persisted_source_generation = None;
        drop(state);
        self.inner.changed.notify_waiters();
    }

    /// 使旧周期任务失去发布权，并在统一 deadline 内尽力写当前 source。
    pub(crate) async fn shutdown(&self, deadline: Deadline) -> CacheSnapshotShutdownSummary {
        let source = {
            let _publish = self
                .inner
                .publish
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.stopped {
                return CacheSnapshotShutdownSummary {
                    completed: true,
                    ..CacheSnapshotShutdownSummary::default()
                };
            }
            state.stopped = true;
            state.source.generation = state.source.generation.saturating_add(1);
            state.status.generation = state.source.generation;
            state.status.condition = if state.source.settings.enabled {
                CacheSnapshotCondition::Writing
            } else {
                CacheSnapshotCondition::Stopped
            };
            state.source.clone()
        };
        self.inner.changed.notify_waiters();
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.abort();
            let _ = tokio::time::timeout(deadline.remaining(Instant::now()), task).await;
        }
        if !source.settings.enabled {
            return CacheSnapshotShutdownSummary {
                completed: true,
                ..CacheSnapshotShutdownSummary::default()
            };
        }
        if deadline.is_expired(Instant::now()) {
            set_shutdown_status(
                &self.inner,
                source.generation,
                None,
                CacheSnapshotFailure::Timeout,
            );
            return CacheSnapshotShutdownSummary {
                attempted: true,
                error: Some(CacheSnapshotFailure::Timeout),
                ..CacheSnapshotShutdownSummary::default()
            };
        }
        let reset_generation = source.store.snapshot_reset_generation();
        match spawn_snapshot_write(
            Arc::clone(&self.inner),
            source,
            reset_generation,
            deadline,
            true,
        )
        .await
        {
            Ok(summary) => {
                set_shutdown_success(&self.inner, summary);
                CacheSnapshotShutdownSummary {
                    completed: true,
                    attempted: true,
                    written: true,
                    error: None,
                }
            }
            Err(CacheSnapshotError::Superseded) => CacheSnapshotShutdownSummary {
                attempted: true,
                error: Some(CacheSnapshotFailure::Unavailable),
                ..CacheSnapshotShutdownSummary::default()
            },
            Err(error) => {
                let failure = failure(&error);
                set_shutdown_status(&self.inner, self.status().generation, None, failure);
                CacheSnapshotShutdownSummary {
                    attempted: true,
                    error: Some(failure),
                    ..CacheSnapshotShutdownSummary::default()
                }
            }
        }
    }

    #[cfg(test)]
    async fn snapshot_now(&self) -> Result<Option<CacheSnapshotWriteSummary>, CacheSnapshotError> {
        snapshot_current(&self.inner).await
    }
}

impl Drop for CacheSnapshotOwner {
    fn drop(&mut self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.stopped = true;
            state.source.generation = state.source.generation.saturating_add(1);
        }
        if let Ok(task) = self.task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}

async fn run_periodic(inner: Weak<OwnerInner>) {
    loop {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let (stopped, enabled, interval) = {
            let state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                state.stopped,
                state.source.settings.enabled,
                state.source.settings.interval,
            )
        };
        if stopped {
            return;
        }
        if !enabled {
            inner.changed.notified().await;
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                let _ = snapshot_current(&inner).await;
            }
            _ = inner.changed.notified() => {}
        }
    }
}

async fn snapshot_current(
    inner: &Arc<OwnerInner>,
) -> Result<Option<CacheSnapshotWriteSummary>, CacheSnapshotError> {
    let (source, reset_generation) = {
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopped || !state.source.settings.enabled {
            return Ok(None);
        }
        let source_generation = state.source.store.snapshot_generation();
        if state.last_persisted_source_generation == Some(source_generation) {
            return Ok(None);
        }
        state.status.condition = CacheSnapshotCondition::Writing;
        (
            state.source.clone(),
            state.source.store.snapshot_reset_generation(),
        )
    };
    let source_generation = source.store.snapshot_generation();
    let deadline = Deadline::new(Instant::now() + SNAPSHOT_OPERATION_TIMEOUT);
    match spawn_snapshot_write(
        Arc::clone(inner),
        source.clone(),
        reset_generation,
        deadline,
        false,
    )
    .await
    {
        Ok(summary) => {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.source.generation == source.generation && !state.stopped {
                state.status.condition = CacheSnapshotCondition::Idle;
                state.status.file_bytes = Some(summary.bytes);
                state.status.last_success_at_utc_millis = Some(summary.generated_at_utc_millis);
                state.status.last_error = None;
                state.last_persisted_source_generation = (source.store.snapshot_generation()
                    == source_generation)
                    .then_some(source_generation);
            }
            Ok(Some(summary))
        }
        Err(CacheSnapshotError::Superseded) => Ok(None),
        Err(error) => {
            let failure = failure(&error);
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.source.generation == source.generation && !state.stopped {
                state.status.condition = CacheSnapshotCondition::Failed;
                state.status.last_error = Some(failure);
            }
            Err(error)
        }
    }
}

async fn spawn_snapshot_write(
    inner: Arc<OwnerInner>,
    source: SnapshotSource,
    reset_generation: u64,
    deadline: Deadline,
    allow_stopped: bool,
) -> Result<CacheSnapshotWriteSummary, CacheSnapshotError> {
    tokio::task::spawn_blocking(move || {
        let mut publish_guard = None;
        let result = write_cache_snapshot_if_current(
            &source.store,
            &source.settings.path,
            SNAPSHOT_BATCH_SIZE,
            deadline,
            || {
                let guard = inner
                    .publish
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !snapshot_publish_is_current(&inner, &source, reset_generation, allow_stopped) {
                    return Ok(false);
                }
                validate_managed_path(&source.settings.path, &source.settings.protected_paths)
                    .map_err(|_| CacheSnapshotError::Unavailable)?;
                publish_guard = Some(guard);
                Ok(true)
            },
        );
        drop(publish_guard);
        result
    })
    .await
    .map_err(|_| CacheSnapshotError::Unavailable)?
}

/// 调用方持有 publish 仲裁锁；显式失效与 owner 切换都必须让旧临时文件失效。
fn snapshot_publish_is_current(
    inner: &OwnerInner,
    source: &SnapshotSource,
    reset_generation: u64,
    allow_stopped: bool,
) -> bool {
    let current = inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    current.source.generation == source.generation
        && Arc::ptr_eq(&current.source.store, &source.store)
        && (!current.stopped || allow_stopped)
        && source.store.snapshot_reset_generation() == reset_generation
}

async fn recover_snapshot(
    store: Arc<MokaCacheStore>,
    path: PathBuf,
    deadline: Deadline,
) -> RecoveryResult {
    let opened = tokio::task::spawn_blocking(move || {
        open_cache_snapshot(&path, SNAPSHOT_FILE_LIMIT_BYTES, deadline)
    })
    .await;
    let mut reader = match opened {
        Ok(Ok(Some(reader))) => reader,
        Ok(Ok(None)) => {
            return RecoveryResult {
                complete: true,
                ..RecoveryResult::default()
            };
        }
        Ok(Err(error)) => {
            return RecoveryResult {
                error: Some(failure(&error)),
                ..RecoveryResult::default()
            };
        }
        Err(_) => {
            return RecoveryResult {
                error: Some(CacheSnapshotFailure::Unavailable),
                ..RecoveryResult::default()
            };
        }
    };
    let file_bytes = Some(reader.snapshot_bytes());
    let last_success_at_utc_millis = Some(reader.generated_at_utc_millis());
    let mut insert_failures = 0_u64;
    while !reader.is_complete() {
        let decoded = tokio::task::spawn_blocking(move || {
            let batch = reader.next_batch(SNAPSHOT_BATCH_SIZE, deadline);
            (reader, batch)
        })
        .await;
        let (next_reader, batch) = match decoded {
            Ok((reader, Ok(batch))) => (reader, batch),
            Ok((reader, Err(error))) => {
                return RecoveryResult {
                    summary: reader.summary(),
                    skipped: insert_failures,
                    file_bytes,
                    last_success_at_utc_millis,
                    error: Some(failure(&error)),
                    ..RecoveryResult::default()
                };
            }
            Err(_) => {
                return RecoveryResult {
                    skipped: insert_failures,
                    file_bytes,
                    last_success_at_utc_millis,
                    error: Some(CacheSnapshotFailure::Unavailable),
                    ..RecoveryResult::default()
                };
            }
        };
        reader = next_reader;
        for (key, record) in batch {
            match store
                .compare_and_swap(key, CacheCondition::Absent, record.entry, deadline)
                .await
            {
                Ok(CacheWriteOutcome::Inserted(_) | CacheWriteOutcome::Replaced(_)) => {}
                Ok(CacheWriteOutcome::Conflict(_) | CacheWriteOutcome::RejectedQuality) => {
                    insert_failures = insert_failures.saturating_add(1)
                }
                Err(error) => {
                    insert_failures = insert_failures.saturating_add(1);
                    if matches!(error.class(), PortErrorClass::Timeout) {
                        let mut summary = reader.summary();
                        summary.loaded = store.stats().entries;
                        return RecoveryResult {
                            summary,
                            skipped: insert_failures,
                            file_bytes,
                            last_success_at_utc_millis,
                            error: Some(CacheSnapshotFailure::Timeout),
                            ..RecoveryResult::default()
                        };
                    }
                }
            }
        }
    }
    let mut summary = reader.summary();
    let decoded = summary.loaded;
    summary.loaded = store.stats().entries;
    let skipped = insert_failures.saturating_add(decoded.saturating_sub(summary.loaded));
    RecoveryResult {
        summary,
        skipped,
        file_bytes,
        last_success_at_utc_millis,
        complete: true,
        error: (skipped > 0).then_some(CacheSnapshotFailure::ResourceExhausted),
    }
}

fn set_shutdown_success(inner: &OwnerInner, summary: CacheSnapshotWriteSummary) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.status.condition = CacheSnapshotCondition::Stopped;
    state.status.file_bytes = Some(summary.bytes);
    state.status.last_success_at_utc_millis = Some(summary.generated_at_utc_millis);
    state.status.last_error = None;
}

fn set_shutdown_status(
    inner: &OwnerInner,
    generation: u64,
    file_bytes: Option<u64>,
    failure: CacheSnapshotFailure,
) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.source.generation == generation {
        state.status.condition = CacheSnapshotCondition::Failed;
        state.status.file_bytes = file_bytes.or(state.status.file_bytes);
        state.status.last_error = Some(failure);
    }
}

fn failure(error: &CacheSnapshotError) -> CacheSnapshotFailure {
    match error {
        CacheSnapshotError::Timeout => CacheSnapshotFailure::Timeout,
        CacheSnapshotError::Unavailable | CacheSnapshotError::Superseded => {
            CacheSnapshotFailure::Unavailable
        }
        CacheSnapshotError::Corrupt => CacheSnapshotFailure::Corrupt,
        CacheSnapshotError::Incompatible => CacheSnapshotFailure::Incompatible,
        CacheSnapshotError::ResourceExhausted => CacheSnapshotFailure::ResourceExhausted,
        CacheSnapshotError::Io(_) => CacheSnapshotFailure::Io,
    }
}

fn validate_managed_path(
    path: &Path,
    protected_paths: &[PathBuf],
) -> Result<(), CacheSnapshotOwnerBuildError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(CacheSnapshotOwnerBuildError::InvalidPath);
    }
    reject_linked_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || is_link_or_reparse(&metadata) => {
            return Err(CacheSnapshotOwnerBuildError::InvalidPath);
        }
        Ok(_) => {
            opened_file_identity(path, true)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(CacheSnapshotOwnerBuildError::InvalidPath),
    }
    for protected in protected_paths {
        if path_eq(path, protected) || same_existing_file(path, protected) {
            return Err(CacheSnapshotOwnerBuildError::InvalidPath);
        }
    }
    Ok(())
}

fn reject_linked_components(path: &Path) -> Result<(), CacheSnapshotOwnerBuildError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_link_or_reparse(&metadata) || current != path && !metadata.is_dir() {
                    return Err(CacheSnapshotOwnerBuildError::InvalidPath);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(CacheSnapshotOwnerBuildError::InvalidPath),
        }
    }
    Ok(())
}

fn same_existing_file(left: &Path, right: &Path) -> bool {
    matches!(
        (
            opened_file_identity(left, false),
            opened_file_identity(right, false)
        ),
        (Ok(left), Ok(right)) if left == right
    )
}

#[cfg(windows)]
fn opened_file_identity(
    path: &Path,
    reject_hard_links: bool,
) -> Result<String, CacheSnapshotOwnerBuildError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };

    let file = fs::File::open(path).map_err(|_| CacheSnapshotOwnerBuildError::InvalidPath)?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    let mut id = std::mem::MaybeUninit::<FILE_ID_INFO>::zeroed();
    // 有效 File 句柄与准确结构长度；仅在两个 Win32 调用成功后读取结构。
    unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) == 0
            || GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                id.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            ) == 0
        {
            return Err(CacheSnapshotOwnerBuildError::InvalidPath);
        }
        let info = info.assume_init();
        if reject_hard_links && info.nNumberOfLinks != 1 {
            return Err(CacheSnapshotOwnerBuildError::InvalidPath);
        }
        let id = id.assume_init();
        Ok(format!(
            "{}:{:x?}",
            id.VolumeSerialNumber, id.FileId.Identifier
        ))
    }
}

#[cfg(unix)]
fn opened_file_identity(
    path: &Path,
    reject_hard_links: bool,
) -> Result<String, CacheSnapshotOwnerBuildError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path).map_err(|_| CacheSnapshotOwnerBuildError::InvalidPath)?;
    if reject_hard_links && metadata.nlink() != 1 {
        return Err(CacheSnapshotOwnerBuildError::InvalidPath);
    }
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(any(windows, unix)))]
fn opened_file_identity(
    path: &Path,
    _reject_hard_links: bool,
) -> Result<String, CacheSnapshotOwnerBuildError> {
    path.canonicalize()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| CacheSnapshotOwnerBuildError::InvalidPath)
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn path_eq(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use crate::cache::{CACHE_KEY_FORMAT_VERSION, canonical_checksum, write_cache_snapshot};
    use crate::dns::{CanonicalQuery, CanonicalResponse, DnsMessageId};
    use crate::ports::cache::{
        CACHE_ENTRY_FORMAT_VERSION, CacheEntry, CacheInvalidation, CacheKey, CacheNamespace,
        CacheQuality, CacheResponseClass, CacheUpstreamProvenance,
    };

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn root() -> PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns")
            .join("p2-cache-owner-tests")
            .join(format!("{}-{id}", std::process::id()))
    }

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn key(value: &[u8]) -> CacheKey {
        CacheKey {
            namespace: CacheNamespace::Global,
            encoded: Arc::from(value),
            format_version: CACHE_KEY_FORMAT_VERSION,
        }
    }

    fn entry(expires_at: Instant) -> Arc<CacheEntry> {
        let mut query = Message::new(0, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(
            Name::from_str("owner.example.").unwrap(),
            RecordType::A,
        ));
        let canonical_query = CanonicalQuery::from_message(query.clone()).unwrap();
        let mut response = Message::response(0, OpCode::Query);
        response.add_query(query.queries[0].clone());
        let response =
            CanonicalResponse::from_message(response, &canonical_query, DnsMessageId::new(0))
                .unwrap();
        let checksum = canonical_checksum(&response).unwrap();
        Arc::new(CacheEntry {
            response: Arc::new(response),
            upstream: CacheUpstreamProvenance::direct_from_validated_config_id("owner").unwrap(),
            inserted_at: Instant::now(),
            expires_at,
            stale_until: None,
            response_class: CacheResponseClass::NoData,
            producer_revision: RuntimeRevision(1),
            quality: CacheQuality::Negative,
            checksum,
            format_version: CACHE_ENTRY_FORMAT_VERSION,
        })
    }

    async fn insert(store: &MokaCacheStore, value: &[u8]) {
        store
            .compare_and_swap(
                key(value),
                CacheCondition::Absent,
                entry(Instant::now() + Duration::from_secs(30)),
                deadline(),
            )
            .await
            .unwrap();
    }

    fn settings(path: PathBuf, interval: Duration) -> CacheSnapshotSettings {
        CacheSnapshotSettings::new(true, path, interval, Vec::new()).unwrap()
    }

    #[tokio::test]
    async fn shutdown_snapshot_recovers_into_a_new_owner_without_sqlite() {
        let root = root();
        let path = root.join("cache.snapshot");
        let first = Arc::new(MokaCacheStore::with_max_weight(1024 * 1024).unwrap());
        let first_owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            Arc::clone(&first),
            settings(path.clone(), Duration::from_secs(3600)),
            deadline(),
        )
        .await
        .unwrap();
        insert(&first, b"restart").await;
        let shutdown = first_owner.shutdown(deadline()).await;
        assert!(shutdown.completed);
        assert!(shutdown.written);

        let second = Arc::new(MokaCacheStore::with_max_weight(1024 * 1024).unwrap());
        let second_owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            Arc::clone(&second),
            settings(path, Duration::from_secs(3600)),
            deadline(),
        )
        .await
        .unwrap();
        assert_eq!(second_owner.status().recovery.loaded, 1);
        assert!(second_owner.status().last_success_at_utc_millis.is_some());
        assert!(
            second
                .get(&key(b"restart"), deadline())
                .await
                .unwrap()
                .is_some()
        );
        assert!(second_owner.shutdown(deadline()).await.completed);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reload_generation_rejects_an_old_in_flight_publish() {
        let root = root();
        let old_path = root.join("old.snapshot");
        let new_path = root.join("new.snapshot");
        let old_store = Arc::new(MokaCacheStore::new());
        insert(&old_store, b"old").await;
        let owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            Arc::clone(&old_store),
            settings(old_path.clone(), Duration::from_secs(3600)),
            deadline(),
        )
        .await
        .unwrap();
        let old_source = owner.inner.state.lock().unwrap().source.clone();
        let old_reset_generation = old_source.store.snapshot_reset_generation();
        let new_store = Arc::new(MokaCacheStore::new());
        insert(&new_store, b"new").await;
        let prepared = owner.prepare_switch(
            RuntimeRevision(2),
            Arc::clone(&new_store),
            settings(new_path.clone(), Duration::from_secs(3600)),
        );
        owner.publish_switch(prepared);

        assert!(matches!(
            spawn_snapshot_write(
                Arc::clone(&owner.inner),
                old_source,
                old_reset_generation,
                deadline(),
                false
            )
            .await,
            Err(CacheSnapshotError::Superseded)
        ));
        assert!(!old_path.exists());
        assert!(owner.snapshot_now().await.unwrap().is_some());
        assert!(new_path.exists());
        assert_eq!(owner.status().owner_revision, RuntimeRevision(2));
        assert_eq!(owner.status().generation, 2);
        assert!(owner.shutdown(deadline()).await.completed);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn explicit_clear_invalidates_a_captured_snapshot_generation() {
        let root = root();
        let path = root.join("clear.snapshot");
        let store = Arc::new(MokaCacheStore::new());
        insert(&store, b"must-not-revive").await;
        let owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            Arc::clone(&store),
            settings(path.clone(), Duration::from_secs(3600)),
            deadline(),
        )
        .await
        .unwrap();
        let source = owner.inner.state.lock().unwrap().source.clone();
        let reset_generation = store.snapshot_reset_generation();

        assert_eq!(
            store
                .invalidate(CacheInvalidation::All, deadline())
                .await
                .unwrap(),
            1
        );
        assert!(!snapshot_publish_is_current(
            &owner.inner,
            &source,
            reset_generation,
            false
        ));
        assert!(owner.snapshot_now().await.unwrap().is_some());
        assert!(owner.shutdown(deadline()).await.completed);

        let recovered = Arc::new(MokaCacheStore::new());
        let recovered_owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            Arc::clone(&recovered),
            settings(path, Duration::from_secs(3600)),
            deadline(),
        )
        .await
        .unwrap();
        assert_eq!(recovered_owner.status().recovery.loaded, 0);
        assert!(
            recovered
                .get(&key(b"must-not-revive"), deadline())
                .await
                .unwrap()
                .is_none()
        );
        assert!(recovered_owner.shutdown(deadline()).await.completed);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn periodic_worker_writes_changes_and_skips_an_unchanged_generation() {
        let root = root();
        let path = root.join("periodic.snapshot");
        let store = Arc::new(MokaCacheStore::new());
        let owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            Arc::clone(&store),
            settings(path.clone(), Duration::from_millis(20)),
            deadline(),
        )
        .await
        .unwrap();
        insert(&store, b"periodic").await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !path.exists()
                || owner.status().condition != CacheSnapshotCondition::Idle
                || owner.status().last_success_at_utc_millis.is_none()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let first_success = owner.status().last_success_at_utc_millis;
        assert!(owner.snapshot_now().await.unwrap().is_none());
        assert_eq!(owner.status().last_success_at_utc_millis, first_success);
        assert!(owner.shutdown(deadline()).await.completed);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn smaller_memory_budget_reports_partial_recovery() {
        let root = root();
        let path = root.join("budget.snapshot");
        let source = MokaCacheStore::with_max_weight(4096).unwrap();
        insert(&source, &[b'a'; 128]).await;
        insert(&source, &[b'b'; 128]).await;
        write_cache_snapshot(&source, &path, 1, deadline()).unwrap();

        let target = Arc::new(MokaCacheStore::with_max_weight(250).unwrap());
        let owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            Arc::clone(&target),
            settings(path, Duration::from_secs(3600)),
            deadline(),
        )
        .await
        .unwrap();
        let status = owner.status();
        assert_eq!(status.recovery.loaded, 1);
        assert_eq!(status.recovery_skipped, 1);
        assert_eq!(
            status.last_error,
            Some(CacheSnapshotFailure::ResourceExhausted)
        );
        assert!(owner.shutdown(deadline()).await.completed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_path_rejects_lexical_and_hard_link_aliases() {
        let root = root();
        fs::create_dir_all(&root).unwrap();
        let protected = root.join("statistics.sqlite3");
        fs::write(&protected, b"protected").unwrap();
        assert!(matches!(
            CacheSnapshotSettings::new(
                true,
                protected.clone(),
                Duration::from_secs(1),
                vec![protected.clone()]
            ),
            Err(CacheSnapshotOwnerBuildError::InvalidPath)
        ));

        let alias = root.join("cache.snapshot");
        fs::hard_link(&protected, &alias).unwrap();
        assert!(matches!(
            CacheSnapshotSettings::new(true, alias, Duration::from_secs(1), vec![protected]),
            Err(CacheSnapshotOwnerBuildError::InvalidPath)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn corrupt_snapshot_degrades_to_cold_start_and_expired_shutdown_is_bounded() {
        let root = root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("corrupt.snapshot");
        fs::write(&path, [0_u8; 60]).unwrap();
        let store = Arc::new(MokaCacheStore::new());
        let owner = CacheSnapshotOwner::start(
            RuntimeRevision(1),
            store,
            settings(path, Duration::from_secs(3600)),
            deadline(),
        )
        .await
        .unwrap();
        assert_eq!(owner.status().condition, CacheSnapshotCondition::Failed);
        assert_eq!(
            owner.status().last_error,
            Some(CacheSnapshotFailure::Corrupt)
        );
        let shutdown = owner.shutdown(Deadline::new(Instant::now())).await;
        assert!(!shutdown.completed);
        assert_eq!(shutdown.error, Some(CacheSnapshotFailure::Timeout));
        fs::remove_dir_all(root).unwrap();
    }
}
