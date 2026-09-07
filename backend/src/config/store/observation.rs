//! 受管配置的有界只读观测；观测 token 不构成 filesystem compare-and-swap。

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use serde::Serialize;

use crate::config::contract::MAX_CONFIG_BYTES;
use crate::config::migrate::deterministic_hash;

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
        deterministic_hash(&serde_json::to_vec(self).expect("file observation is serializable"))
    }

    pub(crate) fn matches_content(&self, fingerprint: &str) -> bool {
        let matches = |file: &FileObservation| {
            matches!(file,
            FileObservation::Readable { fingerprint: actual, .. } if actual == fingerprint)
        };
        matches(&self.source) && self.derived.as_ref().is_none_or(matches)
    }
}

fn observe(path: &Path) -> FileObservation {
    match read_file(path) {
        Ok((identity, bytes)) => FileObservation::Readable {
            identity,
            fingerprint: deterministic_hash(&bytes),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => FileObservation::Missing,
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => FileObservation::Oversized,
        Err(_) => FileObservation::Unreadable,
    }
}

/// 源与父目录都不能经过 symlink/reparse point；读取前后检查路径、文件身份和元数据。
/// 非合作外部编辑器仍可能在检查后修改文件，写入方必须重新观测，不能复用旧 token 代替检查。
pub(super) fn read_file(path: &Path) -> io::Result<(String, Vec<u8>)> {
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
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
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

fn check_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute managed path required"));
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
fn file_identity(file: &File) -> io::Result<String> {
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
        if info.nNumberOfLinks != 1 {
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
fn file_identity(file: &File) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if metadata.nlink() != 1 {
        return Err(io::Error::other("hard-linked managed file rejected"));
    }
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}
