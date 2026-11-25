//! 源配置文件的只读观测；不解析候选、不触碰运行态或会话。

use std::path::PathBuf;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::config::disk::{self, DiskObservation};
use crate::config::store::ConfigStore;

/// 一次带代次的观测；代次来自读取开始前的 store，用于丢弃被 store 写入抢先的过期结果。
type Tagged = (u64, DiskObservation);

pub(super) struct ConfigFileWatcher {
    source: PathBuf,
    candidate: Option<Tagged>,
    /// 最近一次上报的稳定观测；仅用于无 store 时的变化日志。
    reported: Option<DiskObservation>,
    reading: Option<JoinHandle<Tagged>>,
}

impl ConfigFileWatcher {
    pub(super) fn new(source: PathBuf) -> Self {
        Self {
            source,
            candidate: None,
            reported: None,
            reading: None,
        }
    }

    /// 服务循环不等待文件 I/O；在途读取结束前不排队另一个读取。
    ///
    /// 连续两次读取得到相同（同代次、同内容）的观测才算稳定，用于应对编辑器分步写入。稳定后每轮
    /// 都上报，是否变化由 store 判断；首次稳定状态也上报，不能把启动后的外改默认为已同步。
    pub(super) async fn poll(
        &mut self,
        generation: u64,
    ) -> Result<Option<Tagged>, tokio::task::JoinError> {
        if self
            .reading
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Ok(None);
        }
        let current = match self.reading.take() {
            Some(task) => match task.await {
                Ok(observation) => Some(observation),
                Err(error) => {
                    self.candidate = None;
                    return Err(error);
                }
            },
            None => None,
        };
        let source = self.source.clone();
        self.reading = Some(tokio::task::spawn_blocking(move || {
            (generation, disk::observe(&source))
        }));
        Ok(current.and_then(|current| self.accept(current)))
    }

    fn accept(&mut self, current: Tagged) -> Option<Tagged> {
        let stable = self.candidate.as_ref() == Some(&current);
        self.candidate = Some(current.clone());
        stable.then_some(current)
    }

    /// 停止调度并有界等待已开始的只读任务；超时不等于 OS 文件读取已取消。
    pub(super) async fn finish(&mut self, timeout: Duration) -> bool {
        let Some(mut reading) = self.reading.take() else {
            return true;
        };
        matches!(tokio::time::timeout(timeout, &mut reading).await, Ok(Ok(_)))
    }
}

/// 生产服务循环只调用此只读入口，不持有修改 DnsService 的能力。
///
/// 有 store 时把稳定观测交给 store（过期代次被丢弃，状态变化由 store 记录日志）；WebUI 未启用
/// 时没有 store，只在稳定观测变化时记录一条日志。
pub(super) async fn report_config_files(
    watcher: &tokio::sync::Mutex<ConfigFileWatcher>,
    store: Option<&ConfigStore>,
) {
    let generation = store.map_or(0, ConfigStore::disk_generation);
    let mut watcher = watcher.lock().await;
    match watcher.poll(generation).await {
        Ok(Some((generation, observation))) => match store {
            Some(store) => store.observe_disk(generation, observation),
            None if watcher.reported.as_ref() != Some(&observation) => {
                if watcher.reported.is_some() {
                    tracing::warn!(
                        event = "configuration_file_observed",
                        component = "application",
                        result = "not_reloaded",
                        "configuration_file_observed"
                    );
                }
                watcher.reported = Some(observation);
            }
            None => {}
        },
        Ok(None) => {}
        Err(_) => tracing::warn!(
            event = "configuration_observation_task_failed",
            component = "application",
            result = "kept_previous_observation",
            "configuration_observation_task_failed"
        ),
    }
}

#[cfg(test)]
mod tests;
