//! 源配置文件的有界读取、摘要与同目录原子替换。
//!
//! 只比较正文摘要，不比较文件身份、mtime 或权限；不写 journal、不加跨进程锁。进程崩溃时
//! 目标文件要么是旧正文、要么是新正文，残留的固定名临时文件会在下次写入时被覆盖。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::contract::MAX_CONFIG_BYTES;
use super::document::digest;
use crate::diagnostics::Diagnostic;

/// watcher 与写入方对源文件的一次观测。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DiskObservation {
    Readable {
        digest: String,
    },
    Missing,
    /// 无法读取、超过 4 MiB 或不是 UTF-8。
    Unreadable,
}

/// 磁盘源文件与内存配置的关系；只比较正文摘要（I3）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiskStatus {
    InSync,
    /// 外部编辑，或上次写盘失败。
    Modified,
    Missing,
    Unreadable,
}

impl DiskStatus {
    /// 日志只投影状态类别，不包含路径、摘要或正文。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in_sync",
            Self::Modified => "modified",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
        }
    }
}

impl DiskObservation {
    /// 与内存配置摘要比较得到磁盘状态。
    pub(crate) fn status(&self, config_digest: &str) -> DiskStatus {
        match self {
            Self::Readable { digest } if digest == config_digest => DiskStatus::InSync,
            Self::Readable { .. } => DiskStatus::Modified,
            Self::Missing => DiskStatus::Missing,
            Self::Unreadable => DiskStatus::Unreadable,
        }
    }

    pub(crate) fn matches(&self, config_digest: &str) -> bool {
        self.status(config_digest) == DiskStatus::InSync
    }
}

/// 一次有界读取的结果；正文已确认是 UTF-8。
pub(crate) enum ReadOutcome {
    Text(String),
    Missing,
    Unreadable,
}

impl ReadOutcome {
    pub(crate) fn observation(&self) -> DiskObservation {
        match self {
            Self::Text(text) => DiskObservation::Readable {
                digest: digest(text.as_bytes()),
            },
            Self::Missing => DiskObservation::Missing,
            Self::Unreadable => DiskObservation::Unreadable,
        }
    }
}

/// 按普通文件读取（跟随符号链接），上限 4 MiB。
pub(crate) fn read(path: &Path) -> ReadOutcome {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ReadOutcome::Missing,
        Err(_) => return ReadOutcome::Unreadable,
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return ReadOutcome::Unreadable;
    }
    let mut bytes = Vec::new();
    if Read::by_ref(&mut file)
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_CONFIG_BYTES
    {
        return ReadOutcome::Unreadable;
    }
    match String::from_utf8(bytes) {
        Ok(text) => ReadOutcome::Text(text),
        Err(_) => ReadOutcome::Unreadable,
    }
}

pub(crate) fn observe(path: &Path) -> DiskObservation {
    read(path).observation()
}

/// 源文件是符号链接时返回其真实路径，写入时替换真实文件而不是把链接换成普通文件。
pub(crate) fn resolve_write_path(source: &Path) -> PathBuf {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(source).unwrap_or_else(|_| source.to_owned())
        }
        _ => source.to_owned(),
    }
}

/// 固定名称的同目录临时文件；已存在时截断复用。
pub(crate) fn temp_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_owned());
    target.with_file_name(format!(".{name}.fluxdns-tmp"))
}

/// 同目录原子替换：写临时文件并 `sync_all`，再 rename（Unix 同步父目录）或
/// `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`。
///
/// 沿用目标的 Unix mode 或 Windows DACL，避免内联凭据的访问权限被放宽；目标被设为只读时
/// 拒绝替换，使“只读”在各平台都表现为写盘失败，而不是被 rename 绕过。
pub(crate) fn write_atomic(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let permissions = match fs::metadata(target) {
        Ok(metadata) => {
            if metadata.permissions().readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "configuration file is read-only",
                ));
            }
            Some(Permissions::capture(target, &metadata)?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let temp = temp_path(target);
    let result =
        write_temp(&temp, permissions.as_ref(), bytes).and_then(|()| replace(&temp, target));
    if result.is_err() {
        // 尽力清理；残留文件不影响正确性，下次写入会截断复用。
        let _ = fs::remove_file(&temp);
    }
    result
}

/// 写盘失败的安全诊断：只包含 I/O 类别，不包含路径或底层错误文本。
pub(crate) fn io_diagnostic(code: &'static str, action: &str, error: &io::Error) -> Diagnostic {
    Diagnostic::new(
        "persistence",
        code,
        format!(
            "{action}（I/O 类别：{:?}），请检查文件权限、只读属性、磁盘空间及文件占用。",
            error.kind()
        ),
    )
}

/// 旧版本写盘中途崩溃可能遗留 journal、stage 或 lock 旁文件；新版本只提示，不恢复也不删除。
pub(crate) fn legacy_sidecars(source: &Path) -> Vec<PathBuf> {
    let (Some(parent), Some(name)) = (source.parent(), source.file_name()) else {
        return Vec::new();
    };
    let prefix = format!(".{}.fluxdns-", name.to_string_lossy());
    let temp = temp_path(source);
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path != &temp
                && path
                    .file_name()
                    .is_some_and(|file| file.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    found.sort();
    found
}

/// `run` 启动时调用：与 WebUI 是否启用无关，发现遗留旁文件只记录一条 warning 并保留文件。
///
/// 若旧版本恰在决定提交后、替换源文件前崩溃，本次会加载旧正文，需要人工按告警核对。
pub(crate) fn warn_legacy_sidecars(source: &Path) {
    let legacy = legacy_sidecars(source);
    if legacy.is_empty() {
        return;
    }
    let names = legacy
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        event = "configuration_legacy_sidecars_found",
        component = "config",
        result = "kept_without_recovery",
        count = legacy.len(),
        files = %names,
        "configuration_legacy_sidecars_found"
    );
}

/// 从目标文件捕获的权限；只在进程内使用，不接受外部输入。
struct Permissions {
    #[cfg(unix)]
    mode: u32,
    #[cfg(windows)]
    descriptor: Vec<u32>,
}

impl Permissions {
    #[cfg(unix)]
    fn capture(_: &Path, metadata: &fs::Metadata) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        Ok(Self {
            mode: metadata.permissions().mode() & 0o7777,
        })
    }

    #[cfg(windows)]
    fn capture(path: &Path, _: &fs::Metadata) -> io::Result<Self> {
        Ok(Self {
            descriptor: protected_descriptor(path)?,
        })
    }
}

#[cfg(unix)]
fn write_temp(temp: &Path, permissions: Option<&Permissions>, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = permissions.map_or(0o600, |permissions| permissions.mode);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(temp)?;
    // mode 只在新建时生效；复用旧临时文件时显式收紧到目标权限后再写入正文。
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(windows)]
fn write_temp(temp: &Path, permissions: Option<&Permissions>, bytes: &[u8]) -> io::Result<()> {
    let mut file = match permissions {
        Some(permissions) => {
            // 安全描述符只在创建时生效，因此先删除残留临时文件再以目标 DACL 新建。
            match fs::remove_file(temp) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            create_with_descriptor(temp, &permissions.descriptor)?
        }
        None => OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(temp)?,
    };
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(unix)]
fn replace(temp: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temp, target)?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn replace(temp: &Path, target: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let temp = wide(temp)?;
    let target = wide(target)?;
    // 两个缓冲在调用期间存活且以 NUL 结尾。
    let moved = unsafe {
        MoveFileExW(
            temp.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut path = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if path.contains(&0) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    path.push(0);
    Ok(path)
}

#[cfg(windows)]
fn create_with_descriptor(path: &Path, descriptor: &[u32]) -> io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };

    let mut descriptor = descriptor.to_vec();
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_mut_ptr().cast(),
        bInheritHandle: 0,
    };
    let path = wide(path)?;
    // 描述符缓冲在调用期间存活；CREATE_NEW 不覆盖现有文件，返回后由 File 独占关闭句柄。
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &security,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// 读取目标的自相对安全描述符并标记 DACL 受保护，新文件不再继承父目录 ACE。
#[cfg(windows)]
fn protected_descriptor(path: &Path) -> io::Result<Vec<u32>> {
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetFileSecurityW,
        GetSecurityDescriptorDacl, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        SetSecurityDescriptorControl,
    };
    let path = wide(path)?;
    let information =
        DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION;
    let mut needed = 0;
    // 两次调用读取自相对描述符；使用 u32 对齐缓冲，且限制异常 ACL 的分配。
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            information,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 || needed > 1024 * 1024 {
        return Err(io::Error::other(
            "configuration DACL is unavailable or oversized",
        ));
    }
    let mut descriptor = vec![0u32; (needed as usize).div_ceil(4)];
    let pointer = descriptor.as_mut_ptr().cast();
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl = std::ptr::null_mut();
    unsafe {
        if GetFileSecurityW(path.as_ptr(), information, pointer, needed, &mut needed) == 0
            || GetSecurityDescriptorDacl(pointer, &mut present, &mut acl, &mut defaulted) == 0
            || present == 0
            || acl.is_null()
            || SetSecurityDescriptorControl(pointer, SE_DACL_PROTECTED, SE_DACL_PROTECTED) == 0
        {
            return Err(io::Error::other("configuration DACL cannot be preserved"));
        }
    }
    Ok(descriptor)
}

/// 测试比较 ACE 顺序、主体、权限和有效继承限制；INHERITED_ACE 仅描述来源，比较前清除。
#[cfg(all(test, windows))]
pub(crate) fn access_entries(path: &Path) -> Vec<Vec<u8>> {
    use windows_sys::Win32::Security::{
        ACE_HEADER, GetAce, GetSecurityDescriptorDacl, INHERITED_ACE,
    };
    let mut descriptor = protected_descriptor(path).unwrap();
    let mut acl = std::ptr::null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    // API 验证后的描述符及 ACE 指针只在所属缓冲存活期间读取。
    unsafe {
        assert_ne!(
            GetSecurityDescriptorDacl(
                descriptor.as_mut_ptr().cast(),
                &mut present,
                &mut acl,
                &mut defaulted,
            ),
            0
        );
        assert!(!acl.is_null());
        (0..(*acl).AceCount)
            .map(|index| {
                let mut entry = std::ptr::null_mut();
                assert_ne!(GetAce(acl, u32::from(index), &mut entry), 0);
                let header = &*entry.cast::<ACE_HEADER>();
                assert!(usize::from(header.AceSize) >= std::mem::size_of::<ACE_HEADER>());
                let mut bytes =
                    std::slice::from_raw_parts(entry.cast::<u8>(), usize::from(header.AceSize))
                        .to_vec();
                bytes[1] &= !(INHERITED_ACE as u8);
                bytes
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
