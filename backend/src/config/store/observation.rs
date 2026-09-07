//! 受管配置的有界只读观测；观测 token 不构成 filesystem compare-and-swap。

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::contract::MAX_CONFIG_BYTES;

/// 受管文件版本和操作绑定使用抗碰撞摘要，不能复用旧迁移模块的非密码学 hash。
pub(super) fn sha256_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) enum FileObservation {
    Readable {
        identity: String,
        fingerprint: String,
    },
    Missing,
    Unreadable,
    Oversized,
}

impl FileObservation {
    /// 日志/状态提示只投影文件状态，不包含源内容、路径或凭据。
    pub(crate) fn state(&self) -> &'static str {
        match self {
            Self::Readable { .. } => "readable",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
            Self::Oversized => "oversized",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ManagedObservation {
    pub(crate) source: FileObservation,
    pub(crate) derived: Option<FileObservation>,
}

impl ManagedObservation {
    pub(crate) fn read(source: &Path, derived: Option<&Path>) -> Self {
        Self {
            source: observe(source),
            derived: derived.map(observe),
        }
    }

    pub(crate) fn revision(&self) -> String {
        sha256_digest(&serde_json::to_vec(self).expect("file observation is serializable"))
    }

    pub(crate) fn matches_content(&self, fingerprint: &str) -> bool {
        let matches = |file: &FileObservation| {
            matches!(file,
            FileObservation::Readable { fingerprint: actual, .. } if actual == fingerprint)
        };
        matches(&self.source) && self.derived.as_ref().is_none_or(matches)
    }
}

pub(super) fn observe(path: &Path) -> FileObservation {
    match read_file(path) {
        Ok((identity, bytes)) => FileObservation::Readable {
            identity,
            fingerprint: sha256_digest(&bytes),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => FileObservation::Missing,
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => FileObservation::Oversized,
        Err(_) => FileObservation::Unreadable,
    }
}

/// 源与父目录都不能经过 symlink/reparse point；读取前后检查路径、文件身份和元数据。
/// 非合作外部编辑器仍可能在检查后修改文件，写入方必须重新观测，不能复用旧 token 代替检查。
pub(super) fn read_file(path: &Path) -> io::Result<(String, Vec<u8>)> {
    read_file_limited(path, MAX_CONFIG_BYTES)
}

pub(super) fn read_file_limited(path: &Path, limit: usize) -> io::Result<(String, Vec<u8>)> {
    if limit > MAX_CONFIG_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    check_path(path)?;
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "managed configuration is not a regular file",
        ));
    }
    let identity = file_identity(&file)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::from(io::ErrorKind::FileTooLarge));
    }
    check_path(path)?;
    let reopened = File::open(path)?;
    let after = reopened.metadata()?;
    if identity != file_identity(&reopened)?
        || metadata.len() != after.len()
        || metadata.modified()? != after.modified()?
    {
        return Err(io::Error::other(
            "managed configuration changed while reading",
        ));
    }
    Ok((identity, bytes))
}

pub(super) fn check_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute managed path required"));
    }
    #[cfg(windows)]
    for part in path.components() {
        if let std::path::Component::Normal(name) = part {
            let name = name.to_string_lossy();
            if name.contains(':') || name.ends_with(['.', ' ']) {
                return Err(io::Error::other(
                    "managed path alias or alternate stream rejected",
                ));
            }
        }
    }
    for part in path.ancestors() {
        let metadata = fs::symlink_metadata(part)?;
        if metadata.file_type().is_symlink() || is_reparse(&metadata) {
            return Err(io::Error::other("linked managed path rejected"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse(_: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
pub(super) fn file_identity(file: &File) -> io::Result<String> {
    identity(file, false)
}

/// 目录身份也参与 journal 绑定，不能仅凭词法路径和文件内容接受替换后的父目录。
pub(super) fn directory_identity(path: &Path) -> io::Result<String> {
    check_path(path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::other("managed parent is not a directory"));
    }
    identity(&file, true)
}

#[cfg(windows)]
fn identity(file: &File, directory: bool) -> io::Result<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    let mut id = std::mem::MaybeUninit::<FILE_ID_INFO>::zeroed();
    // 有效 File 句柄与准确结构长度；仅在两个 Win32 调用成功后读取初始化后的结构。
    unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) == 0
            || GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                id.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let info = info.assume_init();
        if !directory && info.nNumberOfLinks != 1 {
            return Err(io::Error::other("hard-linked managed file rejected"));
        }
        let id = id.assume_init();
        Ok(format!(
            "{}:{:x?}",
            id.VolumeSerialNumber, id.FileId.Identifier
        ))
    }
}

#[cfg(unix)]
pub(super) fn file_identity(file: &File) -> io::Result<String> {
    identity(file, false)
}

#[cfg(unix)]
fn identity(file: &File, directory: bool) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if !directory && metadata.nlink() != 1 {
        return Err(io::Error::other("hard-linked managed file rejected"));
    }
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}
