//! 主配置和派生副本的只读提示；不解析候选、不触碰运行态或会话。

use std::path::PathBuf;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::config::store::ConfigStore;
use crate::config::store::active::ActiveError;
use crate::config::store::observation::ManagedObservation;

pub(super) struct ConfigFileWatcher {
    source: PathBuf,
    derived: Option<PathBuf>,
    observed: Option<ManagedObservation>,
    candidate: Option<ManagedObservation>,
    reading: Option<JoinHandle<ManagedObservation>>,
}

impl ConfigFileWatcher {
    pub(super) fn new(source: PathBuf, derived: Option<PathBuf>) -> Self {
        Self {
            source,
            derived,
            observed: None,
            candidate: None,
            reading: None,
        }
    }

    /// 服务循环不等待文件 I/O；在途读取结束前不排队另一个读取。
    ///
    /// 首次稳定状态也上报，不能把 prepare 期间的外改默认为已同步。
    /// 自写归属和差异决策由配置 owner 处理，本层不按事件次数猜测来源。
    pub(super) async fn poll_change(
        &mut self,
    ) -> Result<Option<ManagedObservation>, tokio::task::JoinError> {
        if self
            .reading
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Ok(None);
        }
        let completed = self.reading.take();
        let current = match completed {
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
        let derived = self.derived.clone();
        self.reading = Some(tokio::task::spawn_blocking(move || {
            ManagedObservation::read(&source, derived.as_deref())
        }));
        Ok(current.and_then(|current| self.accept(current)))
    }

    fn accept(&mut self, current: ManagedObservation) -> Option<ManagedObservation> {
        if self.observed.as_ref() == Some(&current) {
            self.candidate = None;
            return None;
        }
        if self.candidate.as_ref() != Some(&current) {
            self.candidate = Some(current);
            return None;
        }
        self.candidate = None;
        self.observed = Some(current.clone());
        Some(current)
    }

    /// ConfigStore 正忙时保留本次稳定事实，下一轮继续投递而不是静默丢失。
    fn retry_report(&mut self, observation: ManagedObservation) {
        if self.observed.as_ref() == Some(&observation) {
            self.observed = None;
            self.candidate = Some(observation);
        }
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
pub(super) async fn report_config_files(
    watcher: &tokio::sync::Mutex<ConfigFileWatcher>,
    store: Option<&ConfigStore>,
) {
    let change = watcher.lock().await.poll_change().await;
    match change {
        Ok(Some(observation)) => {
            if let Some(store) = store
                && let Err(error) = store.record_file_observation(observation.clone())
            {
                if matches!(error, ActiveError::Busy) {
                    watcher.lock().await.retry_report(observation);
                } else {
                    tracing::warn!(
                        event = "configuration_observation_rejected",
                        component = "application",
                        result = "kept_previous_observation",
                        "configuration_observation_rejected"
                    );
                }
                return;
            }
            tracing::warn!(
                event = "configuration_files_observed",
                component = "application",
                result = "not_reloaded",
                observed_file_revision = %observation.revision(),
                source_state = observation.source.state(),
                derived_state = observation.derived.as_ref().map(|file| file.state()),
                "configuration_files_observed"
            );
        }
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
