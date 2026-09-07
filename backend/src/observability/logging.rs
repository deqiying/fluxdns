//! 复用进程 subscriber 和 telemetry 的日志 owner；文件准备不占用 DNS 控制线程。

use std::{
    fs::OpenOptions,
    io,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing_subscriber::{Registry, filter::LevelFilter, reload};

use super::{BOOTSTRAP_FILTER, BOOTSTRAP_OUTPUT, OutputTarget, TelemetryWriter, lock_unpoisoned};
use crate::config::{model::LogLevelDto, resolve::ResolvedLogs};
use crate::dns::Deadline;

type FilterHandle = reload::Handle<LevelFilter, Registry>;

pub(crate) struct LoggingOwner {
    output: Arc<Mutex<OutputTarget>>,
    filter: FilterHandle,
    writer: Arc<TelemetryWriter>,
    current: Mutex<ResolvedLogs>,
    preparation: Arc<Semaphore>,
}

pub(crate) struct PreparedLogging {
    owner: Arc<LoggingOwner>,
    expected: ResolvedLogs,
    next: ResolvedLogs,
    target: Option<OutputTarget>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("logging owner is not initialized")]
    Unavailable,
    #[error("logging output is busy")]
    Busy,
    #[error("logging preparation deadline exceeded")]
    Timeout,
    #[error("logging owner configuration changed")]
    Conflict,
    #[error("logging output preparation failed: {0}")]
    Prepare(#[source] io::Error),
    #[error("logging preparation worker failed")]
    Worker,
    #[error("logging filter reload failed")]
    Filter,
}

#[derive(Debug)]
pub(crate) enum LoggingPublishError<E> {
    Logging(LoggingError),
    Application(E),
    CompensationFailed(E),
}

impl LoggingOwner {
    /// 仅在 bootstrap/final 输出已安装后接管；关闭日志也必须保留同一 writer 和指标。
    pub(crate) fn from_bootstrap(
        current: ResolvedLogs,
        writer: Arc<TelemetryWriter>,
    ) -> Result<Arc<Self>, LoggingError> {
        let owner = Arc::new(Self {
            output: BOOTSTRAP_OUTPUT
                .get()
                .cloned()
                .ok_or(LoggingError::Unavailable)?,
            filter: BOOTSTRAP_FILTER
                .get()
                .cloned()
                .ok_or(LoggingError::Unavailable)?,
            writer,
            current: Mutex::new(current),
            preparation: Arc::new(Semaphore::new(1)),
        });
        owner.set_writer_filter(log_filter(&lock_unpoisoned(&owner.current)));
        Ok(owner)
    }

    pub(crate) fn matches(&self, config: &ResolvedLogs, writer: &Arc<TelemetryWriter>) -> bool {
        *lock_unpoisoned(&self.current) == *config && Arc::ptr_eq(&self.writer, writer)
    }

    /// 同一进程最多一份日志准备；超时只放弃等待，阻塞 worker 仍持有槽位直到真实退出。
    /// 预开可能创建空日志文件，但不会切换输出，也不按路径删除已有或新建文件。
    pub(crate) async fn prepare(
        self: &Arc<Self>,
        expected: &ResolvedLogs,
        next: ResolvedLogs,
        deadline: Deadline,
    ) -> Result<PreparedLogging, LoggingError> {
        if deadline.is_expired(Instant::now()) {
            return Err(LoggingError::Timeout);
        }
        let permit = Arc::clone(&self.preparation)
            .try_acquire_owned()
            .map_err(|_| LoggingError::Busy)?;
        if *lock_unpoisoned(&self.current) != *expected {
            return Err(LoggingError::Conflict);
        }
        let owner = Arc::clone(self);
        let expected = expected.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let target = if !next.enable {
                Some(OutputTarget::Sink)
            } else if expected.enable && expected.path == next.path {
                None
            } else {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&next.path)
                    .map_err(LoggingError::Prepare)?;
                if !file.metadata().map_err(LoggingError::Prepare)?.is_file() {
                    return Err(LoggingError::Prepare(io::Error::other(
                        "logging output must be a regular file",
                    )));
                }
                Some(OutputTarget::File(file))
            };
            Ok(PreparedLogging {
                owner,
                expected,
                next,
                target,
                _permit: permit,
            })
        });
        tokio::time::timeout(deadline.remaining(Instant::now()), prepared)
            .await
            .map_err(|_| LoggingError::Timeout)?
            .map_err(|_| LoggingError::Worker)?
    }

    fn set_writer_filter(&self, filter: LevelFilter) {
        let mut state = lock_unpoisoned(&self.writer.state);
        state.log_filter = filter;
        // 不让关闭期间或已被新级别排除的排队日志在再次开启时重现；指标/health 保留。
        state.queue.retain(|item| item.matches_log_filter(filter));
    }
}

impl PreparedLogging {
    /// 输出锁和 flush 锁只尝试获取，不等待磁盘 flush；发布闭包必须同步且不执行日志 I/O。
    /// filter 是最后一个可失败的日志步骤，应用失败时恢复旧 filter，输出句柄始终未换。
    pub(crate) fn publish_with<T, E>(
        mut self,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, LoggingPublishError<E>> {
        let mut current = self
            .owner
            .current
            .try_lock()
            .map_err(|_| LoggingPublishError::Logging(LoggingError::Busy))?;
        if *current != self.expected {
            return Err(LoggingPublishError::Logging(LoggingError::Conflict));
        }
        let _flush = self
            .owner
            .writer
            .flush_lock
            .try_lock()
            .map_err(|_| LoggingPublishError::Logging(LoggingError::Busy))?;
        let mut output = self
            .owner
            .output
            .try_lock()
            .map_err(|_| LoggingPublishError::Logging(LoggingError::Busy))?;
        self.owner
            .filter
            .reload(log_filter(&self.next))
            .map_err(|_| LoggingPublishError::Logging(LoggingError::Filter))?;
        let result = match publish() {
            Ok(result) => result,
            Err(error) => {
                return Err(
                    if self.owner.filter.reload(log_filter(&self.expected)).is_ok() {
                        LoggingPublishError::Application(error)
                    } else {
                        LoggingPublishError::CompensationFailed(error)
                    },
                );
            }
        };
        self.owner.set_writer_filter(log_filter(&self.next));
        if let Some(target) = self.target.take() {
            *output = target;
        }
        *current = self.next;
        Ok(result)
    }
}

fn log_filter(config: &ResolvedLogs) -> LevelFilter {
    if !config.enable {
        return LevelFilter::OFF;
    }
    match config.level {
        LogLevelDto::Trace => LevelFilter::TRACE,
        LogLevelDto::Debug => LevelFilter::DEBUG,
        LogLevelDto::Info => LevelFilter::INFO,
        LogLevelDto::Warn => LevelFilter::WARN,
        LogLevelDto::Error => LevelFilter::ERROR,
    }
}

#[cfg(test)]
impl LoggingOwner {
    pub(crate) fn for_test(
        config: ResolvedLogs,
    ) -> (Arc<Self>, Arc<TelemetryWriter>, tracing::Dispatch) {
        use tracing_subscriber::layer::SubscriberExt;
        let target = if config.enable {
            OutputTarget::File(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&config.path)
                    .unwrap(),
            )
        } else {
            OutputTarget::Sink
        };
        let output = Arc::new(Mutex::new(target));
        let writer = Arc::new(
            TelemetryWriter::new(
                1024,
                Arc::new(super::StructuredTelemetryOutput::shared(Arc::clone(
                    &output,
                ))),
            )
            .unwrap(),
        );
        let (filter, handle) = reload::Layer::new(log_filter(&config));
        let subscriber =
            tracing_subscriber::registry()
                .with(filter)
                .with(super::TypedTracingLayer {
                    writer: Arc::clone(&writer),
                });
        let owner = Arc::new(Self {
            output,
            filter: handle,
            writer: Arc::clone(&writer),
            current: Mutex::new(config),
            preparation: Arc::new(Semaphore::new(1)),
        });
        owner.set_writer_filter(log_filter(&lock_unpoisoned(&owner.current)));
        (owner, writer, tracing::Dispatch::new(subscriber))
    }
}

#[cfg(test)]
mod tests;
