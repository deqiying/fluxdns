//! 解析详情 UTC 日分片的 layout、受限连接和 writer 生命周期。

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use time::{Date, Month};
use tokio::sync::{
    OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, OwnedSemaphorePermit, RwLock,
    Semaphore, mpsc,
};

use crate::dns::Deadline;
use crate::ports::{PortError, PortErrorClass};

use super::detail_query::DetailQueryState;
use super::resolve_log::ResolveDetailRecord;
use super::sqlite::{
    SqliteResolveDetailFlushSummary, SqliteResolveDetailRunSummary, apply_resolve_records,
};
use super::statistics::day_utc;

pub const DETAIL_SHARD_LAYOUT_VERSION: u32 = 1;
pub const DEFAULT_MAX_ACTIVE_DETAIL_SHARDS: usize = 4;

pub(super) const UNIX_EPOCH_JULIAN_DAY: i32 = 2_440_588;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DetailSqlTestStage {
    BeforeSql,
    BeforeCommit,
    AfterCommit,
}

#[cfg(test)]
type DetailTestGate =
    Arc<Mutex<Option<(DetailSqlTestStage, Arc<crate::ports::testing::TestGate>)>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum DetailShardStoreBuildError {
    #[error(
        "detail shard root must be an absolute managed directory separated from protected files"
    )]
    InvalidPath,
    #[error("detail shard active connection limit must be greater than zero")]
    InvalidConnectionLimit,
    #[error("detail shard process identity could not be generated")]
    Entropy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ShardedResolveDetailWriterBuildError {
    #[error("sharded resolve detail queue capacity must be greater than zero")]
    ZeroCapacity,
    #[error("sharded resolve detail batch size must be greater than zero")]
    ZeroBatchSize,
}

#[derive(Default)]
struct DetailShardState {
    stopping: bool,
    retired_before: Option<i32>,
    retiring_days: HashSet<i32>,
}

/// 进程级详情分片 registry；每个 lease 至多持有一个单连接 SQLite pool。
pub struct DetailShardStore {
    root: Arc<PathBuf>,
    protected_paths: Arc<Vec<PathBuf>>,
    day_locks: Mutex<HashMap<i32, Weak<tokio::sync::Mutex<()>>>>,
    retention_gate: Arc<RwLock<()>>,
    permits: Arc<Semaphore>,
    max_active: usize,
    active: Arc<AtomicUsize>,
    peak_active: Arc<AtomicUsize>,
    state: Mutex<DetailShardState>,
    pub(super) query_state: DetailQueryState,
    #[cfg(test)]
    detail_test_gate: DetailTestGate,
    #[cfg(test)]
    fail_reclaim_delete_once: Arc<AtomicBool>,
}

/// 已校验归属且受 registry 约束的单日 SQLite lease。
#[allow(dead_code)] // BC-09/10 将消费只读 pool、日期定位和显式关闭入口。
pub(crate) struct DetailShardLease {
    day_utc: i32,
    path: PathBuf,
    pool: SqlitePool,
    active: Arc<AtomicUsize>,
    _retention_guard: OwnedRwLockReadGuard<()>,
    _day_guard: OwnedMutexGuard<()>,
    _permit: OwnedSemaphorePermit,
}

/// 冻结所有分片 lease，供共同水位事务成功后原子发布详情逻辑边界。
pub(crate) struct DetailRetentionPublicationLease<'a> {
    store: &'a DetailShardStore,
    _guard: OwnedRwLockWriteGuard<()>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DetailStorageSample {
    pub bytes: u64,
    pub shard_days: Vec<i32>,
}

/// 先发布退役状态、再等待既有读写 lease 排空后的独占日锁。
pub(crate) struct DetailShardRetirementLease {
    day_utc: i32,
    path: PathBuf,
    root: Arc<PathBuf>,
    protected_paths: Arc<Vec<PathBuf>>,
    #[cfg(test)]
    fail_reclaim_delete_once: Arc<AtomicBool>,
    _day_guard: OwnedMutexGuard<()>,
}

impl DetailShardStore {
    /// 创建不触碰磁盘的 registry；目录仅在首个写 lease 时创建。
    pub fn new(
        root: PathBuf,
        protected_paths: Vec<PathBuf>,
        max_active: usize,
    ) -> Result<Self, DetailShardStoreBuildError> {
        if max_active == 0 || u32::try_from(max_active).is_err() {
            return Err(DetailShardStoreBuildError::InvalidConnectionLimit);
        }
        validate_managed_root(&root, &protected_paths)?;
        let query_state = DetailQueryState::new()?;
        Ok(Self {
            root: Arc::new(root),
            protected_paths: Arc::new(protected_paths),
            day_locks: Mutex::new(HashMap::new()),
            retention_gate: Arc::new(RwLock::new(())),
            permits: Arc::new(Semaphore::new(max_active)),
            max_active,
            active: Arc::new(AtomicUsize::new(0)),
            peak_active: Arc::new(AtomicUsize::new(0)),
            state: Mutex::new(DetailShardState::default()),
            query_state,
            #[cfg(test)]
            detail_test_gate: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            fail_reclaim_delete_once: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    pub fn shard_path(&self, day_utc: i32) -> Result<PathBuf, PortError> {
        let name = format_shard_file_name(day_utc).ok_or_else(|| {
            PortError::new(PortErrorClass::InvalidInput, "detail_shard.path")
                .with_safe_context("UTC day is outside supported range")
        })?;
        Ok(self.root.join(name))
    }

    /// 获取写 lease；同一天串行，不同天共享全局连接上限。
    #[allow(dead_code)] // 直接 lease 仅供存储契约测试；生产批写通过 write_records 统一处理水位丢弃。
    pub(crate) async fn acquire_write(
        &self,
        day_utc: i32,
        deadline: Deadline,
    ) -> Result<DetailShardLease, PortError> {
        self.acquire(day_utc, true, deadline).await?.ok_or_else(|| {
            PortError::new(PortErrorClass::Unavailable, "detail_shard.write_lease")
                .with_safe_context("shard is retired")
        })
    }

    /// 获取只读 lease；缺失或已逻辑退役的日期返回 `None`，且绝不创建目录或文件。
    #[allow(dead_code)] // BC-09 跨分片 read model 的接入点。
    pub(crate) async fn acquire_read(
        &self,
        day_utc: i32,
        deadline: Deadline,
    ) -> Result<Option<DetailShardLease>, PortError> {
        self.acquire(day_utc, false, deadline).await
    }

    async fn acquire(
        &self,
        day_utc: i32,
        writable: bool,
        deadline: Deadline,
    ) -> Result<Option<DetailShardLease>, PortError> {
        let retention_guard = deadline_future(
            deadline,
            "detail_shard.retention_gate",
            Arc::clone(&self.retention_gate).read_owned(),
        )
        .await?;
        if !self.day_is_visible(day_utc) {
            return Ok(None);
        }
        let path = self.shard_path(day_utc)?;
        if !writable && !path.exists() {
            return Ok(None);
        }
        let day_lock = self.day_lock(day_utc);
        let day_guard =
            deadline_future(deadline, "detail_shard.day_lease", day_lock.lock_owned()).await?;
        let permit = deadline_future(
            deadline,
            "detail_shard.connection_lease",
            Arc::clone(&self.permits).acquire_owned(),
        )
        .await?
        .map_err(|_| {
            PortError::new(PortErrorClass::Unavailable, "detail_shard.connection_lease")
        })?;
        if !self.day_is_visible(day_utc) {
            return Ok(None);
        }
        if writable {
            prepare_managed_root(&self.root, &self.protected_paths).await?;
        } else if !path.exists() {
            return Ok(None);
        }
        validate_shard_path(&path, &self.root, &self.protected_paths)?;
        if writable && path.metadata().is_ok_and(|metadata| metadata.len() > 0) {
            let preflight = open_shard_pool(&path, false, deadline).await?;
            let validation = validate_shard_metadata(&preflight, day_utc, deadline).await;
            preflight.close().await;
            validation?;
            validate_shard_path(&path, &self.root, &self.protected_paths)?;
        }
        let pool = open_shard_pool(&path, writable, deadline).await?;
        let initialized = if writable {
            initialize_or_validate_shard(&pool, day_utc, deadline).await
        } else {
            validate_shard_metadata(&pool, day_utc, deadline).await
        };
        if let Err(error) = initialized {
            pool.close().await;
            return Err(error);
        }
        validate_shard_path(&path, &self.root, &self.protected_paths)?;
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak_active.fetch_max(active, Ordering::AcqRel);
        Ok(Some(DetailShardLease {
            day_utc,
            path,
            pool,
            active: Arc::clone(&self.active),
            _retention_guard: retention_guard,
            _day_guard: day_guard,
            _permit: permit,
        }))
    }

    pub(crate) async fn write_records(
        &self,
        day_utc: i32,
        records: &[ResolveDetailRecord],
        deadline: Deadline,
    ) -> Result<SqliteResolveDetailFlushSummary, PortError> {
        if records.is_empty() {
            return Ok(SqliteResolveDetailFlushSummary::default());
        }
        let lease = match self.acquire(day_utc, true, deadline).await? {
            Some(lease) => lease,
            None if self.day_is_retired(day_utc) => {
                return Ok(SqliteResolveDetailFlushSummary {
                    committed: 0,
                    evicted: 0,
                    dropped: records.len() as u64,
                });
            }
            None => {
                return Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "detail_shard.write_lease",
                ));
            }
        };
        let pool = lease.pool.clone();
        #[cfg(test)]
        self.pause_detail_for_test(DetailSqlTestStage::BeforeSql)
            .await;
        let write =
            deadline_future(deadline, "detail_shard.write", async move {
                let mut transaction = pool.begin().await.map_err(|_| {
                    PortError::new(PortErrorClass::Unavailable, "detail_shard.write")
                })?;
                let row_ids = apply_resolve_records(&mut transaction, records).await?;
                #[cfg(test)]
                self.pause_detail_for_test(DetailSqlTestStage::BeforeCommit)
                    .await;
                transaction.commit().await.map_err(|_| {
                    PortError::new(PortErrorClass::Unavailable, "detail_shard.write")
                })?;
                self.query_state.publish_commit(day_utc, records, &row_ids);
                #[cfg(test)]
                self.pause_detail_for_test(DetailSqlTestStage::AfterCommit)
                    .await;
                Ok(SqliteResolveDetailFlushSummary {
                    committed: records.len() as u64,
                    evicted: 0,
                    dropped: 0,
                })
            })
            .await
            .and_then(|result| result);
        let close = lease.close(deadline).await;
        match (write, close) {
            (Ok(summary), Ok(())) => Ok(summary),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    /// 发布逻辑退役并取得该日独占锁；物理 checkpoint/delete 由保留协调器执行。
    pub(crate) async fn begin_retirement(
        &self,
        day_utc: i32,
        deadline: Deadline,
    ) -> Result<DetailShardRetirementLease, PortError> {
        {
            let mut state = self.state.lock().unwrap();
            if state.stopping {
                return Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "detail_shard.retire",
                ));
            }
            state.retiring_days.insert(day_utc);
        }
        let path = self.shard_path(day_utc)?;
        let guard = deadline_future(
            deadline,
            "detail_shard.retire",
            self.day_lock(day_utc).lock_owned(),
        )
        .await?;
        Ok(DetailShardRetirementLease {
            day_utc,
            path,
            root: Arc::clone(&self.root),
            protected_paths: Arc::clone(&self.protected_paths),
            #[cfg(test)]
            fail_reclaim_delete_once: Arc::clone(&self.fail_reclaim_delete_once),
            _day_guard: guard,
        })
    }

    /// 从已持久化共同水位恢复逻辑可见范围；水位只能向前推进。
    pub(crate) fn publish_retired_before(&self, day_utc: i32) {
        let mut state = self.state.lock().unwrap();
        if state.retired_before.is_none_or(|current| day_utc > current) {
            state.retired_before = Some(day_utc);
            self.query_state.advance_retention_revision();
        }
    }

    /// 等待现有读写 lease 全部归还，并在持锁期间阻止新 lease。
    pub(crate) async fn begin_retention_publication(
        &self,
        deadline: Deadline,
    ) -> Result<DetailRetentionPublicationLease<'_>, PortError> {
        let guard = deadline_future(
            deadline,
            "detail_shard.retention_publication",
            Arc::clone(&self.retention_gate).write_owned(),
        )
        .await?;
        if self.state.lock().unwrap().stopping {
            return Err(PortError::new(
                PortErrorClass::Unavailable,
                "detail_shard.retention_publication",
            ));
        }
        Ok(DetailRetentionPublicationLease {
            store: self,
            _guard: guard,
        })
    }

    /// 冻结本轮受管详情主文件与 WAL 大小；不统计 SHM、备份或其他文件。
    pub(crate) fn sample_managed_storage(
        &self,
        deadline: Deadline,
    ) -> Result<DetailStorageSample, PortError> {
        self.sample_managed_storage_for_days(None, deadline)
    }

    /// 只统计指定 manifest 日期的主文件与 WAL；调用方持有 retention run lock，避免回收交错。
    pub(crate) fn sample_pending_storage(
        &self,
        days: &BTreeSet<i32>,
        deadline: Deadline,
    ) -> Result<DetailStorageSample, PortError> {
        self.sample_managed_storage_for_days(Some(days), deadline)
    }

    fn sample_managed_storage_for_days(
        &self,
        days: Option<&BTreeSet<i32>>,
        deadline: Deadline,
    ) -> Result<DetailStorageSample, PortError> {
        if deadline.is_expired(Instant::now()) {
            return Err(PortError::new(
                PortErrorClass::Timeout,
                "detail_shard.sample",
            ));
        }
        let entries = match fs::read_dir(self.root()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DetailStorageSample::default());
            }
            Err(_) => {
                return Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "detail_shard.sample",
                ));
            }
        };
        let mut sample = DetailStorageSample::default();
        for entry in entries {
            if deadline.is_expired(Instant::now()) {
                return Err(PortError::new(
                    PortErrorClass::Timeout,
                    "detail_shard.sample",
                ));
            }
            let entry = entry
                .map_err(|_| PortError::new(PortErrorClass::Unavailable, "detail_shard.sample"))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(day_utc) = parse_shard_file_name(&name) else {
                continue;
            };
            if days.is_some_and(|days| !days.contains(&day_utc)) {
                continue;
            }
            let path = entry.path();
            validate_shard_path(&path, &self.root, &self.protected_paths)?;
            let main_bytes = fs::metadata(&path)
                .map_err(|_| PortError::new(PortErrorClass::Unavailable, "detail_shard.sample"))?
                .len();
            let mut wal = path.as_os_str().to_os_string();
            wal.push("-wal");
            let wal_bytes = match fs::metadata(Path::new(&wal)) {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(_) => {
                    return Err(PortError::new(
                        PortErrorClass::Unavailable,
                        "detail_shard.sample",
                    ));
                }
            };
            sample.bytes = sample
                .bytes
                .checked_add(main_bytes)
                .and_then(|value| value.checked_add(wal_bytes))
                .ok_or_else(|| {
                    PortError::new(PortErrorClass::ResourceExhausted, "detail_shard.sample")
                })?;
            sample.shard_days.push(day_utc);
        }
        sample.shard_days.sort_unstable();
        sample.shard_days.dedup();
        Ok(sample)
    }

    /// 拒绝新 lease，并等待所有活动连接在同一 deadline 内归还。
    pub async fn shutdown(&self, deadline: Deadline) -> Result<(), PortError> {
        self.state.lock().unwrap().stopping = true;
        let permits = deadline_future(
            deadline,
            "detail_shard.shutdown",
            Arc::clone(&self.permits).acquire_many_owned(self.max_active as u32),
        )
        .await?
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "detail_shard.shutdown"))?;
        drop(permits);
        Ok(())
    }

    fn day_lock(&self, day_utc: i32) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.day_locks.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&day_utc).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(day_utc, Arc::downgrade(&lock));
        lock
    }

    fn day_is_visible(&self, day_utc: i32) -> bool {
        let state = self.state.lock().unwrap();
        !state.stopping
            && state
                .retired_before
                .is_none_or(|retired_before| day_utc >= retired_before)
            && !state.retiring_days.contains(&day_utc)
    }

    fn day_is_retired(&self, day_utc: i32) -> bool {
        self.state
            .lock()
            .unwrap()
            .retired_before
            .is_some_and(|retired_before| day_utc < retired_before)
    }

    pub(super) fn retired_before(&self) -> Option<i32> {
        self.state.lock().unwrap().retired_before
    }

    #[cfg(test)]
    pub(super) fn set_detail_test_gate(
        &self,
        stage: DetailSqlTestStage,
        gate: Arc<crate::ports::testing::TestGate>,
    ) {
        *self.detail_test_gate.lock().unwrap() = Some((stage, gate));
    }

    #[cfg(test)]
    pub(super) fn fail_next_reclaim_delete_for_test(&self) {
        self.fail_reclaim_delete_once.store(true, Ordering::Release);
    }

    #[cfg(test)]
    async fn pause_detail_for_test(&self, stage: DetailSqlTestStage) {
        let gate = {
            let mut slot = self.detail_test_gate.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|(expected, _)| *expected == stage)
            {
                slot.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.pause().await;
        }
    }

    #[cfg(test)]
    fn active_connections(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn peak_active_connections(&self) -> usize {
        self.peak_active.load(Ordering::Acquire)
    }
}

impl DetailRetentionPublicationLease<'_> {
    /// 必须在统计水位事务提交后调用；持有 write guard 时没有详情 lease 可穿越边界。
    pub(crate) fn publish(self, retired_before_day_utc: i32) {
        self.store.publish_retired_before(retired_before_day_utc);
    }
}

#[allow(dead_code)] // lease 的读口与定位信息由 BC-09/10 分别消费。
impl DetailShardLease {
    pub(crate) fn day_utc(&self) -> i32 {
        self.day_utc
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub(crate) async fn close(self, deadline: Deadline) -> Result<(), PortError> {
        deadline_future(deadline, "detail_shard.close", self.pool.close()).await
    }
}

impl Drop for DetailShardLease {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[allow(dead_code)] // day/path 元数据仅供分片 lease 契约测试；生产回收直接消费 reclaim。
impl DetailShardRetirementLease {
    pub(crate) fn day_utc(&self) -> i32 {
        self.day_utc
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 在独占日锁内 checkpoint 并删除规范主文件及 SQLite sidecar；缺失文件视为幂等成功。
    pub(crate) async fn reclaim(self, deadline: Deadline) -> Result<u64, PortError> {
        validate_shard_path(&self.path, &self.root, &self.protected_paths)?;
        let reclaimed_bytes = managed_shard_bytes(&self.path)?;
        if self.path.exists() {
            let pool = open_existing_writable_shard_pool(&self.path, deadline).await?;
            let validation = validate_shard_metadata(&pool, self.day_utc, deadline).await;
            if let Err(error) = validation {
                pool.close().await;
                return Err(error);
            }
            let checkpoint = deadline_future(deadline, "detail_shard.checkpoint", async {
                sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(TRUNCATE)")
                    .fetch_one(&pool)
                    .await
            })
            .await?
            .map_err(|_| PortError::new(PortErrorClass::Unavailable, "detail_shard.checkpoint"))
            .and_then(|(busy, _, _)| {
                if busy == 0 {
                    Ok(())
                } else {
                    Err(PortError::new(
                        PortErrorClass::Unavailable,
                        "detail_shard.checkpoint",
                    ))
                }
            });
            let close = deadline_future(deadline, "detail_shard.reclaim_close", pool.close()).await;
            checkpoint?;
            close?;
        }
        validate_shard_path(&self.path, &self.root, &self.protected_paths)?;
        #[cfg(test)]
        if self.fail_reclaim_delete_once.swap(false, Ordering::AcqRel) {
            return Err(PortError::new(
                PortErrorClass::Unavailable,
                "detail_shard.reclaim_delete",
            ));
        }
        for suffix in ["-wal", "-shm", "-journal", ""] {
            let path = if suffix.is_empty() {
                self.path.clone()
            } else {
                let mut sidecar = self.path.as_os_str().to_os_string();
                sidecar.push(suffix);
                PathBuf::from(sidecar)
            };
            remove_managed_file(&path, deadline).await?;
        }
        Ok(reclaimed_bytes)
    }
}

/// DNS 请求路径仅无等待入队，分片选择与 SQLite I/O 均由 worker 完成。
#[derive(Clone)]
pub struct ShardedResolveDetailWriter {
    sender: mpsc::Sender<ResolveDetailRecord>,
}

pub struct ShardedResolveDetailWorker {
    store: Arc<DetailShardStore>,
    receiver: mpsc::Receiver<ResolveDetailRecord>,
    pending: VecDeque<ResolveDetailRecord>,
    max_batch: usize,
}

impl ShardedResolveDetailWriter {
    pub fn channel(
        store: Arc<DetailShardStore>,
        capacity: usize,
        max_batch: usize,
    ) -> Result<(Self, ShardedResolveDetailWorker), ShardedResolveDetailWriterBuildError> {
        if capacity == 0 {
            return Err(ShardedResolveDetailWriterBuildError::ZeroCapacity);
        }
        if max_batch == 0 {
            return Err(ShardedResolveDetailWriterBuildError::ZeroBatchSize);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self { sender },
            ShardedResolveDetailWorker {
                store,
                receiver,
                pending: VecDeque::new(),
                max_batch,
            },
        ))
    }

    pub(crate) fn try_write(&self, record: ResolveDetailRecord) -> Result<(), PortError> {
        self.sender.try_send(record).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                PortError::new(PortErrorClass::ResourceExhausted, "detail_shard.enqueue")
                    .with_safe_context("queue full")
            }
            mpsc::error::TrySendError::Closed(_) => {
                PortError::new(PortErrorClass::Unavailable, "detail_shard.enqueue")
                    .with_safe_context("worker closed")
            }
        })
    }
}

impl ShardedResolveDetailWorker {
    pub fn pending_len(&self) -> usize {
        self.pending.len().saturating_add(self.receiver.len())
    }

    /// 单个事务只提交队首同一 UTC 日的连续记录，避免跨文件部分提交后重复重放。
    pub async fn flush(
        &mut self,
        deadline: Deadline,
    ) -> Result<SqliteResolveDetailFlushSummary, PortError> {
        while self.pending.len() < self.max_batch {
            match self.receiver.try_recv() {
                Ok(record) => self.pending.push_back(record),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
        let Some(first) = self.pending.front() else {
            return Ok(SqliteResolveDetailFlushSummary::default());
        };
        let day = record_day(first)?;
        let count = self
            .pending
            .iter()
            .take(self.max_batch)
            .take_while(|record| record_day(record).is_ok_and(|value| value == day))
            .count();
        let records = self.pending.iter().take(count).cloned().collect::<Vec<_>>();
        let summary = self.store.write_records(day, &records, deadline).await?;
        for _ in 0..count {
            let _ = self.pending.pop_front();
        }
        Ok(summary)
    }

    pub async fn shutdown(
        mut self,
        deadline: Deadline,
    ) -> Result<SqliteResolveDetailFlushSummary, PortError> {
        self.receiver.close();
        let mut total = SqliteResolveDetailFlushSummary::default();
        while self.pending_len() > 0 {
            merge_flush(&mut total, self.flush(deadline).await?);
        }
        Ok(total)
    }

    pub(crate) async fn run_until_stopped(
        mut self,
        cancellation: crate::dns::Cancellation,
        flush_interval: Duration,
        operation_timeout: Duration,
    ) -> Result<(Self, SqliteResolveDetailRunSummary), PortError> {
        if flush_interval.is_zero() || operation_timeout.is_zero() {
            return Err(
                PortError::new(PortErrorClass::InvalidInput, "detail_shard.run")
                    .with_safe_context("flush interval and operation timeout must be positive"),
            );
        }
        let mut summary = SqliteResolveDetailRunSummary::default();
        let mut interval = tokio::time::interval(flush_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                record = self.receiver.recv() => {
                    let Some(record) = record else { break };
                    self.pending.push_back(record);
                    while self.pending.len() < self.max_batch {
                        match self.receiver.try_recv() {
                            Ok(record) => self.pending.push_back(record),
                            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }
                    if self.pending.len() >= self.max_batch {
                        merge_run_flush(
                            &mut summary,
                            self.flush(Deadline::new(Instant::now() + operation_timeout)).await,
                        );
                    }
                }
                _ = interval.tick() => merge_run_flush(
                    &mut summary,
                    self.flush(Deadline::new(Instant::now() + operation_timeout)).await,
                ),
            }
        }
        self.receiver.close();
        Ok((self, summary))
    }
}

fn merge_flush(
    total: &mut SqliteResolveDetailFlushSummary,
    value: SqliteResolveDetailFlushSummary,
) {
    total.committed = total.committed.saturating_add(value.committed);
    total.evicted = total.evicted.saturating_add(value.evicted);
    total.dropped = total.dropped.saturating_add(value.dropped);
}

fn merge_run_flush(
    summary: &mut SqliteResolveDetailRunSummary,
    result: Result<SqliteResolveDetailFlushSummary, PortError>,
) {
    match result {
        Ok(value) => merge_flush(&mut summary.flush, value),
        Err(_) => summary.failed_flushes = summary.failed_flushes.saturating_add(1),
    }
}

fn record_day(record: &ResolveDetailRecord) -> Result<i32, PortError> {
    day_utc(record.occurred_at()).map_err(|_| {
        PortError::new(PortErrorClass::InvalidInput, "detail_shard.day")
            .with_safe_context("event UTC day is outside supported range")
    })
}

pub(super) fn format_shard_file_name(day_utc: i32) -> Option<String> {
    let date = Date::from_julian_day(day_utc.checked_add(UNIX_EPOCH_JULIAN_DAY)?).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}.sqlite3",
        date.year(),
        u8::from(date.month()),
        date.day()
    ))
}

pub(crate) fn parse_shard_file_name(name: &str) -> Option<i32> {
    let date = name.strip_suffix(".sqlite3")?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let parsed = Date::from_calendar_date(year, month, day).ok()?;
    let day_utc = parsed.to_julian_day().checked_sub(UNIX_EPOCH_JULIAN_DAY)?;
    (format_shard_file_name(day_utc).as_deref() == Some(name)).then_some(day_utc)
}

async fn open_shard_pool(
    path: &Path,
    writable: bool,
    deadline: Deadline,
) -> Result<SqlitePool, PortError> {
    let options = if writable {
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(SQLITE_BUSY_TIMEOUT.min(deadline.remaining(Instant::now())))
    } else {
        SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .busy_timeout(SQLITE_BUSY_TIMEOUT.min(deadline.remaining(Instant::now())))
    };
    deadline_future(
        deadline,
        "detail_shard.open",
        SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(deadline.remaining(Instant::now()))
            .connect_with(options),
    )
    .await?
    .map_err(|_| PortError::new(PortErrorClass::Unavailable, "detail_shard.open"))
}

async fn open_existing_writable_shard_pool(
    path: &Path,
    deadline: Deadline,
) -> Result<SqlitePool, PortError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(SQLITE_BUSY_TIMEOUT.min(deadline.remaining(Instant::now())));
    deadline_future(
        deadline,
        "detail_shard.reclaim_open",
        SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(deadline.remaining(Instant::now()))
            .connect_with(options),
    )
    .await?
    .map_err(|_| PortError::new(PortErrorClass::Unavailable, "detail_shard.reclaim_open"))
}

fn managed_shard_bytes(path: &Path) -> Result<u64, PortError> {
    let mut bytes = 0_u64;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let candidate = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            let mut candidate = path.as_os_str().to_os_string();
            candidate.push(suffix);
            PathBuf::from(candidate)
        };
        match fs::metadata(candidate) {
            Ok(metadata) => {
                bytes = bytes.checked_add(metadata.len()).ok_or_else(|| {
                    PortError::new(PortErrorClass::ResourceExhausted, "detail_shard.reclaim")
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "detail_shard.reclaim",
                ));
            }
        }
    }
    Ok(bytes)
}

async fn remove_managed_file(path: &Path, deadline: Deadline) -> Result<(), PortError> {
    if deadline.is_expired(Instant::now()) {
        return Err(PortError::new(
            PortErrorClass::Timeout,
            "detail_shard.reclaim_delete",
        ));
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(PortError::new(
            PortErrorClass::Unavailable,
            "detail_shard.reclaim_delete",
        )),
    }
}

async fn initialize_or_validate_shard(
    pool: &SqlitePool,
    day_utc: i32,
    deadline: Deadline,
) -> Result<(), PortError> {
    deadline_future(deadline, "detail_shard.initialize", async {
        let mut transaction = pool.begin().await.map_err(|_| {
            PortError::new(PortErrorClass::Unavailable, "detail_shard.initialize")
        })?;
        let has_meta = sqlx::query(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'detail_meta' LIMIT 1",
        )
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "detail_shard.initialize"))?
        .is_some();
        if !has_meta {
            let user_tables: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .fetch_one(&mut *transaction)
            .await
            .map_err(|_| {
                PortError::new(PortErrorClass::Unavailable, "detail_shard.initialize")
            })?;
            if user_tables != 0 {
                return Err(
                    PortError::new(PortErrorClass::InvalidInput, "detail_shard.initialize")
                        .with_safe_context("existing file is not a detail shard"),
                );
            }
            for statement in include_str!("../../migrations/detail/0001_detail_shard.sql").split(';') {
                let statement = statement.trim();
                if !statement.is_empty() {
                    sqlx::query(statement)
                        .execute(&mut *transaction)
                        .await
                        .map_err(|_| {
                            PortError::new(
                                PortErrorClass::Unavailable,
                                "detail_shard.initialize",
                            )
                        })?;
                }
            }
            sqlx::query(
                "INSERT INTO detail_meta (singleton, layout_version, day_utc, created_at_utc_millis) \
                 VALUES (1, ?, ?, ?)",
            )
            .bind(i64::from(DETAIL_SHARD_LAYOUT_VERSION))
            .bind(i64::from(day_utc))
            .bind(system_time_utc_millis(SystemTime::now())?)
            .execute(&mut *transaction)
            .await
            .map_err(|_| {
                PortError::new(PortErrorClass::Unavailable, "detail_shard.initialize")
            })?;
            sqlx::query(include_str!(
                "../../migrations/detail/0002_detail_day_guard.sql"
            ))
            .execute(&mut *transaction)
            .await
            .map_err(|_| {
                PortError::new(PortErrorClass::Unavailable, "detail_shard.initialize")
            })?;
        }
        for statement in include_str!("../../migrations/detail/0003_detail_query_indexes.sql").split(';') {
            let statement = statement.trim();
            if !statement.is_empty() {
                sqlx::query(statement)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|_| {
                        PortError::new(PortErrorClass::Unavailable, "detail_shard.initialize")
                    })?;
            }
        }
        validate_shard_metadata_executor(&mut transaction, day_utc).await?;
        let required_objects: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_schema WHERE \
             (type = 'table' AND name = 'resolve_log') OR \
             (type = 'index' AND name = 'resolve_log_event_time_idx') OR \
             (type = 'index' AND name = 'resolve_log_duration_idx') OR \
             (type = 'index' AND name = 'resolve_log_client_id_time_idx') OR \
             (type = 'index' AND name = 'resolve_log_client_ip_time_idx') OR \
             (type = 'index' AND name = 'resolve_log_matched_client_time_idx') OR \
             (type = 'index' AND name = 'resolve_log_qname_time_idx') OR \
             (type = 'trigger' AND name = 'resolve_log_day_guard')",
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "detail_shard.validate"))?;
        validate_required_objects(required_objects)?;
        transaction.commit().await.map_err(|_| {
            PortError::new(PortErrorClass::Unavailable, "detail_shard.initialize")
        })?;
        Ok(())
    })
    .await?
}

async fn validate_shard_metadata(
    pool: &SqlitePool,
    day_utc: i32,
    deadline: Deadline,
) -> Result<(), PortError> {
    deadline_future(deadline, "detail_shard.validate", async {
        let row =
            sqlx::query("SELECT layout_version, day_utc FROM detail_meta WHERE singleton = 1")
                .fetch_one(pool)
                .await
                .map_err(|_| {
                    PortError::new(PortErrorClass::InvalidInput, "detail_shard.validate")
                })?;
        validate_metadata_values(&row, day_utc)?;
        let required_objects: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_schema WHERE \
             (type = 'table' AND name = 'resolve_log') OR \
             (type = 'index' AND name = 'resolve_log_event_time_idx') OR \
             (type = 'index' AND name = 'resolve_log_duration_idx') OR \
             (type = 'index' AND name = 'resolve_log_client_id_time_idx') OR \
             (type = 'index' AND name = 'resolve_log_client_ip_time_idx') OR \
             (type = 'index' AND name = 'resolve_log_matched_client_time_idx') OR \
             (type = 'index' AND name = 'resolve_log_qname_time_idx') OR \
             (type = 'trigger' AND name = 'resolve_log_day_guard')",
        )
        .fetch_one(pool)
        .await
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "detail_shard.validate"))?;
        validate_required_objects(required_objects)
    })
    .await?
}

async fn validate_shard_metadata_executor(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    day_utc: i32,
) -> Result<(), PortError> {
    let row = sqlx::query("SELECT layout_version, day_utc FROM detail_meta WHERE singleton = 1")
        .fetch_one(&mut **transaction)
        .await
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "detail_shard.validate"))?;
    validate_metadata_values(&row, day_utc)
}

fn validate_metadata_values(row: &sqlx::sqlite::SqliteRow, day_utc: i32) -> Result<(), PortError> {
    let layout_version = row
        .try_get::<i64, _>("layout_version")
        .ok()
        .and_then(|value| u32::try_from(value).ok());
    let actual_day = row
        .try_get::<i64, _>("day_utc")
        .ok()
        .and_then(|value| i32::try_from(value).ok());
    if layout_version != Some(DETAIL_SHARD_LAYOUT_VERSION) || actual_day != Some(day_utc) {
        return Err(
            PortError::new(PortErrorClass::InvalidInput, "detail_shard.validate")
                .with_safe_context("detail shard metadata does not match path"),
        );
    }
    Ok(())
}

fn validate_required_objects(count: i64) -> Result<(), PortError> {
    if count != 8 {
        return Err(
            PortError::new(PortErrorClass::InvalidInput, "detail_shard.validate")
                .with_safe_context("detail shard layout is incomplete"),
        );
    }
    Ok(())
}

fn system_time_utc_millis(value: SystemTime) -> Result<i64, PortError> {
    let duration = value
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "detail_shard.timestamp"))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| PortError::new(PortErrorClass::InvalidInput, "detail_shard.timestamp"))
}

async fn deadline_future<F, T>(
    deadline: Deadline,
    operation: &'static str,
    future: F,
) -> Result<T, PortError>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(deadline.remaining(Instant::now()), future)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Timeout, operation))
}

async fn prepare_managed_root(root: &Path, protected_paths: &[PathBuf]) -> Result<(), PortError> {
    validate_managed_root(root, protected_paths).map_err(build_error_to_port)?;
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|_| PortError::new(PortErrorClass::Unavailable, "detail_shard.prepare"))?;
    validate_managed_root(root, protected_paths).map_err(build_error_to_port)
}

fn validate_managed_root(
    root: &Path,
    protected_paths: &[PathBuf],
) -> Result<(), DetailShardStoreBuildError> {
    if !root.is_absolute() || root.file_name().is_none() {
        return Err(DetailShardStoreBuildError::InvalidPath);
    }
    reject_linked_components(root)?;
    match fs::symlink_metadata(root) {
        Ok(metadata) if !metadata.is_dir() || is_link_or_reparse(&metadata) => {
            return Err(DetailShardStoreBuildError::InvalidPath);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(DetailShardStoreBuildError::InvalidPath),
    }
    if protected_paths
        .iter()
        .any(|path| path_eq(root, path) || path_within(path, root))
    {
        return Err(DetailShardStoreBuildError::InvalidPath);
    }
    Ok(())
}

fn validate_shard_path(
    path: &Path,
    root: &Path,
    protected_paths: &[PathBuf],
) -> Result<(), PortError> {
    if path.parent() != Some(root)
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_shard_file_name)
            .is_none()
    {
        return Err(PortError::new(
            PortErrorClass::InvalidInput,
            "detail_shard.path",
        ));
    }
    validate_existing_managed_file(path, protected_paths)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        validate_existing_managed_file(Path::new(&sidecar), protected_paths)?;
    }
    Ok(())
}

fn validate_existing_managed_file(
    path: &Path,
    protected_paths: &[PathBuf],
) -> Result<(), PortError> {
    reject_linked_components(path).map_err(build_error_to_port)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || is_link_or_reparse(&metadata) => {
            return Err(PortError::new(
                PortErrorClass::InvalidInput,
                "detail_shard.path",
            ));
        }
        Ok(_) => {
            opened_file_identity(path, true).map_err(build_error_to_port)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(PortError::new(
                PortErrorClass::Unavailable,
                "detail_shard.path",
            ));
        }
    }
    if protected_paths
        .iter()
        .any(|protected| path_eq(path, protected) || same_existing_file(path, protected))
    {
        return Err(PortError::new(
            PortErrorClass::InvalidInput,
            "detail_shard.path",
        ));
    }
    Ok(())
}

fn build_error_to_port(_: DetailShardStoreBuildError) -> PortError {
    PortError::new(PortErrorClass::InvalidInput, "detail_shard.path")
}

fn reject_linked_components(path: &Path) -> Result<(), DetailShardStoreBuildError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_link_or_reparse(&metadata) || current != path && !metadata.is_dir() {
                    return Err(DetailShardStoreBuildError::InvalidPath);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(DetailShardStoreBuildError::InvalidPath),
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
) -> Result<String, DetailShardStoreBuildError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };

    let file = fs::File::open(path).map_err(|_| DetailShardStoreBuildError::InvalidPath)?;
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
            return Err(DetailShardStoreBuildError::InvalidPath);
        }
        let info = info.assume_init();
        if reject_hard_links && info.nNumberOfLinks != 1 {
            return Err(DetailShardStoreBuildError::InvalidPath);
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
) -> Result<String, DetailShardStoreBuildError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path).map_err(|_| DetailShardStoreBuildError::InvalidPath)?;
    if reject_hard_links && metadata.nlink() != 1 {
        return Err(DetailShardStoreBuildError::InvalidPath);
    }
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(any(windows, unix)))]
fn opened_file_identity(
    path: &Path,
    _reject_hard_links: bool,
) -> Result<String, DetailShardStoreBuildError> {
    path.canonicalize()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| DetailShardStoreBuildError::InvalidPath)
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

fn path_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = path
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        let mut root = root
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase();
        root.push('\\');
        path.starts_with(&root)
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root) && path != root
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use crate::dns::{Deadline, RuntimeRevision, TransportClass};
    use crate::ports::storage::{ResolveEvent, StatsSource};
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};

    use super::{
        DetailShardStore, ShardedResolveDetailWriter, format_shard_file_name, parse_shard_file_name,
    };

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns")
            .join("p2-detail-shard-tests")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn record(day: i32, second: u64) -> crate::storage::ResolveDetailRecord {
        let occurred_at =
            UNIX_EPOCH + Duration::from_secs(u64::try_from(day).unwrap() * 86_400 + second);
        crate::storage::ResolveDetailRecord::from_event(ResolveEvent {
            occurred_at,
            duration_millis: 8,
            dns_core_duration_micros: 250,
            request_digest: Arc::from(format!("request-{day}-{second}")),
            listener_id: Arc::from("udp-main"),
            route_id: None,
            client_id: None,
            client_ip: None,
            client_match_source: None,
            matched_client_id: None,
            client_bucket: None,
            strategy_id: None,
            upstream_id: None,
            upstream_member_id: None,
            upstream_used_id: None,
            matched_rule_source: None,
            matched_resource_id: None,
            matched_rule_ordinal: None,
            resource_version: None,
            transport: TransportClass::Datagram,
            qname: Arc::from("example.test."),
            qtype: 1,
            qclass: 1,
            answers: Vec::new(),
            rcode: 0,
            cancellation_reason: None,
            outcome: OutcomeClass::Success,
            source: StatsSource::Upstream,
            cache_status: CacheStatus::Miss,
            runtime_revision: RuntimeRevision(1),
        })
        .unwrap()
    }

    async fn open_verification(path: &std::path::Path) -> sqlx::SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(path).read_only(true))
            .await
            .unwrap()
    }

    #[test]
    fn shard_file_names_are_canonical_utc_dates() {
        for (day, name) in [
            (0, "1970-01-01.sqlite3"),
            (20_339, "2025-09-08.sqlite3"),
            (20_704, "2026-09-08.sqlite3"),
        ] {
            assert_eq!(format_shard_file_name(day).as_deref(), Some(name));
            assert_eq!(parse_shard_file_name(name), Some(day));
        }
        for invalid in [
            "2026-9-08.sqlite3",
            "2026-09-8.sqlite3",
            "2026-02-30.sqlite3",
            "2026-09-08.db",
            "../2026-09-08.sqlite3",
        ] {
            assert_eq!(parse_shard_file_name(invalid), None, "{invalid}");
        }
    }

    #[tokio::test]
    async fn real_sqlite_writer_routes_cross_day_and_late_records_without_eviction() {
        let root = test_root("cross-day");
        let store = Arc::new(DetailShardStore::new(root.clone(), Vec::new(), 2).unwrap());
        let (writer, worker) =
            ShardedResolveDetailWriter::channel(Arc::clone(&store), 16, 16).unwrap();
        let first_day = 20_704;
        writer.try_write(record(first_day, 86_399)).unwrap();
        writer.try_write(record(first_day + 1, 0)).unwrap();
        writer.try_write(record(first_day, 1)).unwrap();
        let summary = worker.shutdown(deadline()).await.unwrap();
        assert_eq!(summary.committed, 3);
        assert_eq!(summary.evicted, 0);
        assert_eq!(summary.dropped, 0);
        let wrong_day = store
            .write_records(first_day, &[record(first_day + 1, 10)], deadline())
            .await;
        assert!(wrong_day.is_err());

        let read = store
            .acquire_read(first_day, deadline())
            .await
            .unwrap()
            .unwrap();
        let leased_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
            .fetch_one(read.pool())
            .await
            .unwrap();
        assert_eq!(leased_count, 2);
        read.close(deadline()).await.unwrap();

        for (day, expected) in [(first_day, 2_i64), (first_day + 1, 1_i64)] {
            let path = root.join(format_shard_file_name(day).unwrap());
            let pool = open_verification(&path).await;
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resolve_log")
                .fetch_one(&pool)
                .await
                .unwrap();
            let metadata: (i64, i64) =
                sqlx::query_as("SELECT layout_version, day_utc FROM detail_meta")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(count, expected);
            assert_eq!(metadata, (1, i64::from(day)));
            pool.close().await;
        }
        let files = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "sqlite3")
            })
            .count();
        assert_eq!(files, 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn read_lease_does_not_create_missing_shard() {
        let root = test_root("read-missing");
        let store = DetailShardStore::new(root.clone(), Vec::new(), 1).unwrap();
        assert!(
            store
                .acquire_read(20_704, deadline())
                .await
                .unwrap()
                .is_none()
        );
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn leases_bound_connections_and_shutdown_waits_for_release() {
        let root = test_root("leases");
        let store = Arc::new(DetailShardStore::new(root.clone(), Vec::new(), 1).unwrap());
        let first = store.acquire_write(20_704, deadline()).await.unwrap();
        assert_eq!(first.day_utc(), 20_704);
        assert!(first.path().ends_with("2026-09-08.sqlite3"));
        assert_eq!(store.active_connections(), 1);

        let next_store = Arc::clone(&store);
        let next = tokio::spawn(async move { next_store.acquire_write(20_705, deadline()).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!next.is_finished());
        first.close(deadline()).await.unwrap();
        let second = next.await.unwrap().unwrap();
        assert_eq!(store.active_connections(), 1);
        assert_eq!(store.peak_active_connections(), 1);

        let shutdown_store = Arc::clone(&store);
        let shutdown = tokio::spawn(async move { shutdown_store.shutdown(deadline()).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!shutdown.is_finished());
        second.close(deadline()).await.unwrap();
        shutdown.await.unwrap().unwrap();
        assert_eq!(store.active_connections(), 0);
        assert!(store.acquire_write(20_706, deadline()).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn retirement_blocks_recreation_after_existing_lease_drains() {
        let root = test_root("retire");
        let store = Arc::new(DetailShardStore::new(root.clone(), Vec::new(), 2).unwrap());
        let existing = store.acquire_write(20_704, deadline()).await.unwrap();
        let retire_store = Arc::clone(&store);
        let retirement =
            tokio::spawn(async move { retire_store.begin_retirement(20_704, deadline()).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!retirement.is_finished());
        existing.close(deadline()).await.unwrap();
        let retired = retirement.await.unwrap().unwrap();
        assert_eq!(retired.day_utc(), 20_704);
        assert!(retired.path().ends_with("2026-09-08.sqlite3"));
        assert!(
            store
                .acquire_read(20_704, deadline())
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.acquire_write(20_704, deadline()).await.is_err());
        drop(retired);

        store.publish_retired_before(20_706);
        assert!(store.acquire_write(20_705, deadline()).await.is_err());
        let active = store.acquire_write(20_706, deadline()).await.unwrap();
        active.close(deadline()).await.unwrap();
        store.shutdown(deadline()).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_root_rejects_protected_files_inside_it() {
        let root = test_root("protected");
        let protected = root.join("statistics.sqlite3");
        assert!(DetailShardStore::new(root, vec![protected], 1).is_err());
    }

    #[tokio::test]
    async fn existing_foreign_sqlite_is_not_adopted_as_a_shard() {
        let root = test_root("foreign");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("2026-09-08.sqlite3");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE foreign_data (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let store = DetailShardStore::new(root.clone(), Vec::new(), 1).unwrap();
        let error = store.acquire_write(20_704, deadline()).await.err().unwrap();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::InvalidInput
        ));
        let verification = open_verification(&path).await;
        let foreign_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'foreign_data'",
        )
        .fetch_one(&verification)
        .await
        .unwrap();
        let detail_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'detail_meta'",
        )
        .fetch_one(&verification)
        .await
        .unwrap();
        assert_eq!((foreign_count, detail_count), (1, 0));
        verification.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn shard_path_rejects_hard_link_to_protected_file() {
        let root = test_root("hard-link");
        let protected = root.with_extension("protected.sqlite3");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&protected, b"protected").unwrap();
        std::fs::hard_link(&protected, root.join("2026-09-08.sqlite3")).unwrap();
        let store = DetailShardStore::new(root.clone(), vec![protected.clone()], 1).unwrap();
        let error = store.acquire_write(20_704, deadline()).await.err().unwrap();
        assert!(matches!(
            error.class(),
            crate::ports::PortErrorClass::InvalidInput
        ));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(protected).unwrap();
    }
}
