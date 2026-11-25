//! 测试专用的进程内窗口隔离，只在 `cfg(test)` 下编译。
//!
//! 同一个测试二进制里有若干用例会派生 `current_exe()` 子进程（panic 探针、日志
//! bootstrap、时区探针）。`fork`/`clone` 出来的子进程在 `exec` 之前持有父进程 fd 的副本；
//! 派生用例持写锁覆盖 `spawn` 到 `wait` 全程，使这些用例彼此串行。

use std::sync::{RwLock, RwLockWriteGuard};

static CHILD_PROCESS_WINDOW: RwLock<()> = RwLock::new(());

/// 派生并等待子进程期间持有；`wait` 返回时子进程已 `exec`（继承的 fd 已被 CLOEXEC 关闭）。
pub(crate) fn child_process_window() -> RwLockWriteGuard<'static, ()> {
    CHILD_PROCESS_WINDOW
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
