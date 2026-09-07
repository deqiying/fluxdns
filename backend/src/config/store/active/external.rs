//! 固定受管源的差异输入；只读、完整校验并在返回前复核双文件与活动版本。

use std::sync::Arc;

use super::{ActiveError, ActiveSnapshot, ConfigStore, ExpectedRevisions};
use crate::config::{
    contract::ConfigV2,
    store::observation::{FileObservation, ManagedObservation, observe, observe_with_content},
    validate::ConfigErrorKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExternalSourceError {
    Missing,
    Unreadable,
    Oversized,
    Invalid,
    UnsupportedVersion,
}

/// 仅供已鉴权的 Management 白名单投影消费，不包含原文或底层错误信息。
pub(crate) struct ExternalSource {
    pub(crate) expected: ExpectedRevisions,
    pub(crate) active: Arc<ConfigV2>,
    pub(crate) external: Result<ConfigV2, ExternalSourceError>,
}

struct CapturedSource {
    active: ActiveSnapshot,
    observation: ManagedObservation,
    external: Result<ConfigV2, ExternalSourceError>,
}

impl ConfigStore {
    /// 同步有界文件/解析工作必须由后台事务 owner 调度，不直接在 HTTP executor 上执行。
    /// 只读取创建 ConfigStore 时固定的路径；派生副本从不作为第二个候选输入。
    pub(crate) fn external_source(&self) -> Result<ExternalSource, ActiveError> {
        let _transaction = self.transaction.try_lock().map_err(|_| ActiveError::Busy)?;
        let captured = self.capture_external_source()?;
        self.finish_external_source(captured)
    }

    fn capture_external_source(&self) -> Result<CapturedSource, ActiveError> {
        let active = self
            .active
            .try_lock()
            .map_err(|_| ActiveError::Busy)?
            .as_ref()
            .ok_or(ActiveError::Unavailable)?
            .snapshot
            .clone();
        let (source, bytes) = observe_with_content(&self.source_path);
        let observation = ManagedObservation {
            source,
            derived: self.snapshot_path.as_deref().map(observe),
        };
        let external = match bytes {
            Some(bytes) => ConfigV2::parse(&bytes)
                .map_err(|report| {
                    if report
                        .errors
                        .iter()
                        .any(|error| error.kind == ConfigErrorKind::UnsupportedVersion)
                    {
                        ExternalSourceError::UnsupportedVersion
                    } else {
                        ExternalSourceError::Invalid
                    }
                })
                .and_then(|config| {
                    config
                        .resolve_paths(&self.source_path)
                        .map_err(|_| ExternalSourceError::Invalid)?;
                    Ok(config)
                }),
            None => Err(match observation.source {
                FileObservation::Missing => ExternalSourceError::Missing,
                FileObservation::Oversized => ExternalSourceError::Oversized,
                _ => ExternalSourceError::Unreadable,
            }),
        };
        Ok(CapturedSource {
            active,
            observation,
            external,
        })
    }

    fn finish_external_source(
        &self,
        captured: CapturedSource,
    ) -> Result<ExternalSource, ActiveError> {
        let observed = ManagedObservation::read(&self.source_path, self.snapshot_path.as_deref());
        let mut guard = self.active.try_lock().map_err(|_| ActiveError::Busy)?;
        let state = guard.as_mut().ok_or(ActiveError::Unavailable)?;
        // 运行成功回报可以在文件观测期间发布；不能把旧活动源的 diff 配上新 revision。
        if state.snapshot.revision != captured.active.revision {
            return Err(ActiveError::ActiveConflict);
        }
        state.snapshot.observation = observed;
        if state.snapshot.observation != captured.observation {
            return Err(ActiveError::FileConflict);
        }
        Ok(ExternalSource {
            expected: state.snapshot.expected(),
            active: captured.active.config,
            external: captured.external,
        })
    }
}

#[cfg(test)]
mod tests;
