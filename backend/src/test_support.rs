//! 测试专用的进程内窗口隔离，只在 `cfg(test)` 下编译。
//!
//! 同一个测试二进制里有若干用例会派生 `current_exe()` 子进程（panic 探针、日志
//! bootstrap、持久化崩溃矩阵）。Linux 上 `File::try_lock` 由 `flock` 实现，锁挂在
//! open file description 上：`fork`/`clone` 出来的子进程在 `exec` 之前持有父进程 fd 的
//! 副本，这份副本会让并发用例在“释放自己的锁后重新加锁”时看到假 `Busy`。
//! 这里让派生用例持写锁（覆盖 `spawn` 到 `wait` 全程），让加锁断言持读锁，使两者不重叠。

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

static CHILD_PROCESS_WINDOW: RwLock<()> = RwLock::new(());

/// 派生并等待子进程期间持有；`wait` 返回时子进程已 `exec`（继承的 fd 已被 CLOEXEC 关闭）。
pub(crate) fn child_process_window() -> RwLockWriteGuard<'static, ()> {
    CHILD_PROCESS_WINDOW
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 试图加文件锁期间持有；多个用例可并行，不相互阻塞。
pub(crate) fn file_lock_window() -> RwLockReadGuard<'static, ()> {
    CHILD_PROCESS_WINDOW
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
