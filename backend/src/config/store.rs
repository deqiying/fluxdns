//! 配置存储：持有内存配置文档（唯一权威）、最近一次磁盘观测和进程内写锁。
//!
//! save、reload、persist 与首用户 setup 共用一把写锁，取不到立即返回 [`StoreError::Busy`]（I4）。
//! 先应用 Runtime 再写盘（I7）：写盘失败不回滚 Runtime，只表现为磁盘状态 `modified`。
//! 磁盘外改只更新观测，不会自动生效（I5）。
//!
//! 不变量编号 I1–I8 与流程设计见 `docs/architecture/backend/modules/config.md` §12–§14。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use arc_swap::ArcSwap;
use thiserror::Error;

use super::change::{ConfigChange, EditError, build_candidate, expand_redacted_changes};
use super::contract::ConfigV2;
use super::disk::{self, DiskObservation, DiskStatus, ReadOutcome};
use super::document::ConfigDocument;
use super::hash::deterministic_hash;
use super::resolve::{
    ResolvedConfig, ResolvedWebUiUser, SecretValidationError, lexical_normalize, resolve_config_v2,
};
use super::validate::ConfigErrorReport;
use crate::diagnostics::Diagnostic;

/// Runtime 应用结果。`Unknown` 表示回执丢失，由 store 按 Runtime 代次判定。
pub(crate) enum ApplyOutcome {
    Applied(u64),
    Failed(Diagnostic),
    Unknown,
}

/// store 与 Runtime 之间的最小接口；生产由 Management 以 `ServiceControl` 实现，测试使用替身。
pub(crate) trait RuntimeHost: Send + Sync {
    /// 当前 Runtime 代次及其解析配置（启动级字段始终是启动时的值）。
    fn current(&self) -> (u64, Arc<ResolvedConfig>);

    /// prepare 并切换到 `expected + 1`；失败时必须保持旧 Runtime 不变。
    fn apply(
        &self,
        expected: u64,
        config: Arc<ResolvedConfig>,
    ) -> impl Future<Output = ApplyOutcome> + Send;
}

/// 一次保存请求；变更形态与前端 payload 一致。
pub(crate) struct SaveInput {
    pub(crate) changes: Vec<ConfigChange>,
    pub(crate) overwrite_disk: bool,
    pub(crate) confirm_retention_shortening: bool,
}

pub(crate) struct SaveOutcome {
    pub(crate) document: Arc<ConfigDocument>,
    /// `false` 表示配置已在运行但没有写入磁盘，重启后会丢失。
    pub(crate) persisted: bool,
    pub(crate) diagnostic: Option<Diagnostic>,
}

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("another configuration write is in progress")]
    Busy,
    #[error("configuration file differs from the in-memory configuration")]
    FileChanged(DiskStatus),
    #[error("configuration file is missing or unreadable")]
    FileUnreadable(DiskStatus),
    #[error("configuration candidate rejected: {0}")]
    Edit(#[from] EditError),
    #[error("retention shortening requires confirmation")]
    ConfirmationRequired,
    #[error("configuration secret validation failed: {0}")]
    Secret(SecretValidationError),
    #[error("runtime apply failed")]
    Apply(Diagnostic),
    #[error("configuration file write failed")]
    Persist(Diagnostic),
    #[error("webui setup has already completed")]
    AlreadyInitialized,
    #[error("configuration source cannot receive the initial user")]
    SetupRejected,
    #[error("configuration store task was interrupted")]
    Internal,
}

impl From<ConfigErrorReport> for StoreError {
    fn from(report: ConfigErrorReport) -> Self {
        Self::Edit(EditError::Validation(report))
    }
}

/// watcher 与写入方共享的最近一次磁盘观测。
///
/// 每次 store 自己读写磁盘都会递增 `generation`；watcher 在读取前取得代次，代次过期的
/// 观测被丢弃，避免慢读取覆盖 store 刚写入的事实。
struct DiskSlot {
    generation: u64,
    observation: DiskObservation,
}

pub(crate) struct ConfigStore {
    /// 启动时的源文件路径（绝对、词法规范化），用于解析相对路径。
    source_path: PathBuf,
    /// 实际写入的路径；源文件是符号链接时为其真实路径。
    write_path: PathBuf,
    current: ArcSwap<ConfigDocument>,
    disk: Mutex<DiskSlot>,
    lock: tokio::sync::Mutex<()>,
}

impl ConfigStore {
    /// 用 loader 实际读取并通过校验的正文构造 store（`runtime_revision = 1`）。
    ///
    /// 只观测一次磁盘。旧版本遗留旁文件的告警由 `run` 启动路径负责，见 `disk::warn_legacy_sidecars`。
    pub(crate) fn open(source_path: &Path, text: &str, config: ConfigV2) -> Self {
        let source_path = lexical_normalize(source_path);
        let document = ConfigDocument::from_parsed(text, config, 1, Vec::new());
        Self {
            write_path: disk::resolve_write_path(&source_path),
            disk: Mutex::new(DiskSlot {
                generation: 0,
                observation: disk::observe(&source_path),
            }),
            source_path,
            current: ArcSwap::from_pointee(document),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 测试入口：从磁盘读取并严格解析配置后构造 store。
    #[cfg(test)]
    pub(crate) fn open_for_test(source_path: &Path) -> Self {
        Self::open_for_test_at(source_path, 1)
    }

    /// 测试入口：文档绑定到指定 Runtime 代次，供与非初始代次的 Runtime 配套使用。
    #[cfg(test)]
    pub(crate) fn open_for_test_at(source_path: &Path, runtime_revision: u64) -> Self {
        let text = std::fs::read_to_string(source_path).unwrap();
        let config = ConfigV2::parse(text.as_bytes()).unwrap();
        let store = Self::open(source_path, &text, config.clone());
        store.current.store(Arc::new(ConfigDocument::from_parsed(
            &text,
            config,
            runtime_revision,
            Vec::new(),
        )));
        store
    }

    /// 测试入口：替换当前文档的 Runtime 代次与需重启组件，模拟 Runtime 已切换、文档尚未发布
    /// 的窗口或启动级字段已修改的状态。
    #[cfg(test)]
    pub(crate) fn set_document_state_for_test(
        &self,
        runtime_revision: u64,
        restart_required: Vec<&'static str>,
    ) {
        let document = self.current();
        self.current.store(Arc::new(ConfigDocument {
            text: Arc::clone(&document.text),
            digest: document.digest.clone(),
            config: Arc::clone(&document.config),
            runtime_revision,
            restart_required,
        }));
    }

    /// 无锁读取当前文档；所有读接口都从它投影（I1）。
    pub(crate) fn current(&self) -> Arc<ConfigDocument> {
        self.current.load_full()
    }

    /// 最近一次观测与给定文档摘要的比较结果；不读盘。读接口传入同一次 `current()` 的摘要，
    /// 使状态与投影的文档同代。
    pub(crate) fn disk_status(&self, config_digest: &str) -> DiskStatus {
        self.slot().observation.status(config_digest)
    }

    /// watcher 在开始读取前取得的代次；见 [`DiskSlot`]。
    pub(crate) fn disk_generation(&self) -> u64 {
        self.slot().generation
    }

    /// 接受 watcher 的稳定观测；代次已过期时丢弃。只改观测，不改文档或 Runtime。
    pub(crate) fn observe_disk(&self, generation: u64, observation: DiskObservation) {
        let digest = self.current.load().digest.clone();
        let mut slot = self.slot();
        if slot.generation != generation || slot.observation == observation {
            return;
        }
        let before = slot.observation.status(&digest);
        slot.observation = observation;
        let after = slot.observation.status(&digest);
        drop(slot);
        if before != after {
            tracing::warn!(
                event = "configuration_file_observed",
                component = "config",
                result = "not_reloaded",
                disk_status = after.as_str(),
                "configuration_file_observed"
            );
        }
    }

    /// 保存：变更 → 条目级写回 → 校验 → 应用 Runtime → 发布文档 → 写盘。
    ///
    /// 调用方应在独立任务中执行，使 HTTP 断开不会中断已经开始的 Runtime 切换与写盘。
    pub(crate) async fn save<H: RuntimeHost>(
        &self,
        host: &H,
        input: SaveInput,
    ) -> Result<SaveOutcome, StoreError> {
        let _guard = self.lock.try_lock().map_err(|_| StoreError::Busy)?;
        let base = self.current();
        let observed = self.read_observation().await?;
        if !input.overwrite_disk && !observed.matches(&base.digest) {
            return Err(StoreError::FileChanged(observed.status(&base.digest)));
        }
        let source_path = self.source_path.clone();
        let base_text = Arc::clone(&base.text);
        let changes = input.changes;
        let (candidate, resolved) = blocking(move || {
            let expanded = expand_redacted_changes(&base_text, &changes)?;
            let candidate = build_candidate(&base_text, &source_path, &expanded)?;
            let resolved = resolve_config_v2(
                &candidate.config,
                deterministic_hash(candidate.source.as_bytes()),
                &source_path,
            )?
            .resolved;
            Ok::<_, StoreError>((candidate, resolved))
        })
        .await??;
        if !input.confirm_retention_shortening
            && retention_shortened(&base.config, &candidate.config)
        {
            return Err(StoreError::ConfirmationRequired);
        }
        let (revision, restart_required) = apply_runtime(host, resolved).await?;
        let document = Arc::new(ConfigDocument::from_parsed(
            &candidate.source,
            candidate.config,
            revision,
            restart_required,
        ));
        self.current.store(Arc::clone(&document));
        let (persisted, diagnostic) = self
            .persist_saved(&base.digest, &document, input.overwrite_disk)
            .await;
        Ok(SaveOutcome {
            document,
            persisted,
            diagnostic,
        })
    }

    /// 从磁盘重新加载并应用。正文未变时直接返回当前文档；不写镜像。
    pub(crate) async fn reload<H: RuntimeHost>(
        &self,
        host: &H,
    ) -> Result<Arc<ConfigDocument>, StoreError> {
        let _guard = self.lock.try_lock().map_err(|_| StoreError::Busy)?;
        let current = self.current();
        let path = self.source_path.clone();
        let outcome = blocking(move || disk::read(&path)).await?;
        let observation = outcome.observation();
        self.record(observation.clone());
        let ReadOutcome::Text(text) = outcome else {
            return Err(StoreError::FileUnreadable(
                observation.status(&current.digest),
            ));
        };
        if observation.matches(&current.digest) {
            return Ok(current);
        }
        let source_path = self.source_path.clone();
        let (text, config, resolved) = blocking(move || {
            let config = ConfigV2::parse(text.as_bytes())?;
            let resolved =
                resolve_config_v2(&config, deterministic_hash(text.as_bytes()), &source_path)?
                    .resolved;
            Ok::<_, StoreError>((text, config, resolved))
        })
        .await??;
        let (revision, restart_required) = apply_runtime(host, resolved).await?;
        let document = Arc::new(ConfigDocument::from_parsed(
            &text,
            config,
            revision,
            restart_required,
        ));
        self.current.store(Arc::clone(&document));
        Ok(document)
    }

    /// 以内存配置覆盖磁盘，不触碰 Runtime。源文件写失败返回 [`StoreError::Persist`]。
    pub(crate) async fn persist(&self) -> Result<Arc<ConfigDocument>, StoreError> {
        let _guard = self.lock.try_lock().map_err(|_| StoreError::Busy)?;
        let document = self.current();
        match self.write_files(&document).await? {
            Ok(_) => Ok(document),
            Err(diagnostic) => Err(StoreError::Persist(diagnostic)),
        }
    }

    /// 首用户 setup：只在仍无用户且磁盘与内存一致时写入 `webui.users`，不切换 Runtime。
    ///
    /// 认证状态由调用方直接替换，不撤销会话；写盘失败时文档保持不变。
    pub(crate) async fn create_initial_user(
        &self,
        name: &str,
        password_hash: &str,
    ) -> Result<Vec<ResolvedWebUiUser>, StoreError> {
        let _guard = self.lock.try_lock().map_err(|_| StoreError::Busy)?;
        let base = self.current();
        if !base.config.webui.users.is_empty() {
            return Err(StoreError::AlreadyInitialized);
        }
        let observed = self.read_observation().await?;
        if !observed.matches(&base.digest) {
            return Err(StoreError::FileChanged(observed.status(&base.digest)));
        }
        let mut user = yaml_serde::Mapping::new();
        user.insert("name".into(), name.into());
        user.insert("password_hash".into(), password_hash.into());
        let users = yaml_serde::Value::Sequence(vec![yaml_serde::Value::Mapping(user)]);
        let source_path = self.source_path.clone();
        let base_text = Arc::clone(&base.text);
        let expected_name = name.to_owned();
        let (text, config, users) = blocking(move || {
            let text = super::writer::set_webui_users(&base_text, &users)
                .map_err(|_| StoreError::SetupRejected)?;
            let config = ConfigV2::parse(text.as_bytes()).map_err(|_| StoreError::SetupRejected)?;
            let resolved =
                resolve_config_v2(&config, deterministic_hash(text.as_bytes()), &source_path)
                    .map_err(|_| StoreError::SetupRejected)?
                    .resolved;
            let users = resolved.webui.users.clone();
            if users.len() != 1 || users[0].name != expected_name {
                return Err(StoreError::SetupRejected);
            }
            Ok((text, config, users))
        })
        .await??;
        let document = ConfigDocument::from_parsed(
            &text,
            config,
            base.runtime_revision,
            base.restart_required.clone(),
        );
        if let Err(diagnostic) = self.write_files(&document).await? {
            return Err(StoreError::Persist(diagnostic));
        }
        self.current.store(Arc::new(document));
        Ok(users)
    }

    /// 保存后的写盘：再次核对磁盘，仍是保存前正文（或已选择覆盖）才写入。
    async fn persist_saved(
        &self,
        base_digest: &str,
        document: &ConfigDocument,
        overwrite: bool,
    ) -> (bool, Option<Diagnostic>) {
        let observed = match self.read_observation().await {
            Ok(observed) => observed,
            Err(_) => return (false, Some(interrupted_diagnostic())),
        };
        if observed.matches(&document.digest) {
            return (true, None);
        }
        if !overwrite && !observed.matches(base_digest) {
            tracing::warn!(
                event = "configuration_save_not_persisted",
                component = "config",
                result = "file_changed_during_save",
                "configuration_save_not_persisted"
            );
            return (
                false,
                Some(Diagnostic::new(
                    "persistence",
                    "CONFIG_FILE_CHANGED_DURING_SAVE",
                    "配置已生效，但保存期间配置文件被外部修改，未覆盖磁盘；请选择写入磁盘或重新加载。",
                )),
            );
        }
        match self.write_files(document).await {
            Ok(Ok(mirror)) => (true, mirror),
            Ok(Err(diagnostic)) => (false, Some(diagnostic)),
            Err(_) => (false, Some(interrupted_diagnostic())),
        }
    }

    /// 先原子写源文件，再写派生镜像；更新磁盘观测。
    ///
    /// 外层错误只表示阻塞任务被中断。内层 `Err` 是源文件写失败的诊断；`Ok(Some)` 是镜像写失败
    /// 的告警诊断，不影响持久化结论。
    async fn write_files(
        &self,
        document: &ConfigDocument,
    ) -> Result<Result<Option<Diagnostic>, Diagnostic>, StoreError> {
        let write_path = self.write_path.clone();
        let source_path = self.source_path.clone();
        let mirror = mirror_path(&document.config, &self.source_path, &self.write_path);
        let text = Arc::clone(&document.text);
        let digest = document.digest.clone();
        let result = blocking(move || {
            if let Err(error) = disk::write_atomic(&write_path, text.as_bytes()) {
                tracing::warn!(
                    event = "configuration_file_write_failed",
                    component = "config",
                    io_kind = ?error.kind(),
                    "configuration_file_write_failed"
                );
                let diagnostic = disk::io_diagnostic(
                    "CONFIG_FILE_WRITE_FAILED",
                    "配置已生效，但写入配置文件失败",
                    &error,
                );
                return (disk::observe(&source_path), Err(diagnostic));
            }
            let mirror = mirror.and_then(|mirror| {
                disk::write_atomic(&mirror, text.as_bytes())
                    .err()
                    .map(|error| {
                        tracing::warn!(
                            event = "configuration_mirror_write_failed",
                            component = "config",
                            io_kind = ?error.kind(),
                            "configuration_mirror_write_failed"
                        );
                        disk::io_diagnostic(
                            "CONFIG_MIRROR_WRITE_FAILED",
                            "配置文件已写入，但工作目录中的配置镜像写入失败",
                            &error,
                        )
                    })
            });
            (DiskObservation::Readable { digest }, Ok(mirror))
        })
        .await?;
        self.record(result.0);
        Ok(result.1)
    }

    /// store 自己读取源文件并记录观测。
    async fn read_observation(&self) -> Result<DiskObservation, StoreError> {
        let path = self.source_path.clone();
        let observation = blocking(move || disk::observe(&path)).await?;
        self.record(observation.clone());
        Ok(observation)
    }

    fn record(&self, observation: DiskObservation) {
        let mut slot = self.slot();
        slot.generation = slot.generation.wrapping_add(1);
        slot.observation = observation;
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, DiskSlot> {
        self.disk.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 校验凭据、沿用启动级字段后应用 Runtime；返回新代次和需重启生效的组件。
async fn apply_runtime<H: RuntimeHost>(
    host: &H,
    mut resolved: Arc<ResolvedConfig>,
) -> Result<(u64, Vec<&'static str>), StoreError> {
    resolved
        .validate_secret_refs(64 * 1024)
        .map_err(StoreError::Secret)?;
    let (expected, running) = host.current();
    let restart_required = crate::service::process_owned_reload_changes(&running, &resolved);
    // 刚解析出的候选只有这一个引用；拿不到可变引用说明内部状态异常。
    let candidate = Arc::get_mut(&mut resolved).ok_or(StoreError::Internal)?;
    keep_process_owned(candidate, &running);
    match host.apply(expected, resolved).await {
        ApplyOutcome::Applied(revision) => Ok((revision, restart_required)),
        ApplyOutcome::Failed(diagnostic) => Err(StoreError::Apply(diagnostic)),
        // 回执丢失时按 Runtime 代次判定：等于新代次视为已应用，否则视为未应用。
        ApplyOutcome::Unknown if host.current().0 == expected.wrapping_add(1) => {
            Ok((expected + 1, restart_required))
        }
        ApplyOutcome::Unknown => Err(StoreError::Apply(Diagnostic::new(
            "runtime_apply",
            "RUNTIME_OUTCOME_UNKNOWN",
            "运行配置切换结果未知，且当前 Runtime 未切换到新版本；原运行状态保持不变。",
        ))),
    }
}

/// 启动级组件在进程启动时创建，Runtime 换代不会替换它们；候选沿用当前 Runtime 的值，
/// 使 service 的 `RestartRequired` 检查不会触发。
fn keep_process_owned(candidate: &mut ResolvedConfig, running: &ResolvedConfig) {
    candidate.database = running.database.clone();
    candidate.webui.enable = running.webui.enable;
    candidate.webui.address = running.webui.address;
    candidate.webui.port = running.webui.port;
    candidate.webui.public_origin = running.webui.public_origin.clone();
    candidate.dns.resolve_log = running.dns.resolve_log.clone();
}

fn retention_shortened(base: &ConfigV2, next: &ConfigV2) -> bool {
    let old = &base.statistics.retention;
    let new = &next.statistics.retention;
    new.days < old.days
        || new.grace_days < old.grace_days
        || new.reference_size_bytes < old.reference_size_bytes
}

/// 派生镜像 `<work.path>/config.yaml`；与源文件相同或路径无法解析时不写。
fn mirror_path(config: &ConfigV2, source_path: &Path, write_path: &Path) -> Option<PathBuf> {
    let mirror = lexical_normalize(
        &config
            .resolve_paths(source_path)
            .ok()?
            .work
            .join("config.yaml"),
    );
    (mirror != source_path && mirror != write_path).then_some(mirror)
}

fn interrupted_diagnostic() -> Diagnostic {
    Diagnostic::new(
        "persistence",
        "CONFIG_FILE_WRITE_FAILED",
        "配置已生效，但写盘任务被中断，请检查磁盘状态后写入磁盘。",
    )
}

async fn blocking<T: Send + 'static>(
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| StoreError::Internal)
}

#[cfg(test)]
mod tests;
