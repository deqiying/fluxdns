//! journal 使用的受限同目录文件操作；不复用 setup 的宽松路径/权限假设。

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use super::super::observation::{check_path, directory_identity, observe};
use super::{PersistenceError, Stamp};

pub(super) fn parent_identity(path: &Path) -> Result<String, PersistenceError> {
    Ok(directory_identity(
        path.parent().ok_or(PersistenceError::InvalidJournal)?,
    )?)
}

pub(super) fn stamp(path: &Path) -> Result<Stamp, PersistenceError> {
    match observe(path) {
        super::super::observation::FileObservation::Readable {
            identity,
            fingerprint,
        } => Ok(Stamp {
            identity,
            fingerprint,
            permissions: permission_stamp(path)?,
        }),
        _ => Err(PersistenceError::Conflict),
    }
}

/// 锁文件保留为空旁文件；持有 OS 锁而不是以时间推测旧进程是否退出。
pub(super) fn acquire_lock(source: &Path) -> Result<(File, Stamp), PersistenceError> {
    let path = super::sibling(source, "lock");
    parent_identity(&path)?;
    if !path.try_exists()? {
        match write_new(&path, source, b"") {
            Ok(_) => {}
            Err(PersistenceError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    // 锁定范围的内容不能通过另一个句柄读取；空锁文件只核对身份、长度和权限。
    check_path(&path)?;
    let before_file = File::open(&path)?;
    if before_file.metadata()?.len() != 0 {
        return Err(PersistenceError::Conflict);
    }
    let before = Stamp {
        identity: super::super::observation::file_identity(&before_file)?,
        fingerprint: super::sha256_digest(b""),
        permissions: permission_stamp(&path)?,
    };
    drop(before_file);
    if before.fingerprint != super::sha256_digest(b"") {
        return Err(PersistenceError::Conflict);
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options.open(&path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Err(PersistenceError::Busy),
        Err(fs::TryLockError::Error(error)) => return Err(PersistenceError::Io(error)),
    }
    verify_lock(&path, &file, &before)?;
    Ok((file, before))
}

pub(super) fn verify_lock(
    path: &Path,
    file: &File,
    expected: &Stamp,
) -> Result<(), PersistenceError> {
    check_path(path)?;
    let reopened = File::open(path)?;
    if file.metadata()?.len() != 0
        || super::super::observation::file_identity(file)? != expected.identity
        || super::super::observation::file_identity(&reopened)? != expected.identity
        || permission_stamp(path)? != expected.permissions
    {
        return Err(PersistenceError::Conflict);
    }
    Ok(())
}

pub(super) fn require(path: &Path, expected: &Stamp) -> Result<(), PersistenceError> {
    if &stamp(path)? != expected {
        return Err(PersistenceError::Conflict);
    }
    Ok(())
}

/// 先带限制权限创建空文件，再写入敏感 bytes；不能先默认继承再收紧权限。
pub(super) fn write_new(
    path: &Path,
    permissions_from: &Path,
    bytes: &[u8],
) -> Result<Stamp, PersistenceError> {
    check_path(permissions_from)?;
    parent_identity(path)?;
    let mut file = restricted_create(path, permissions_from)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    let observed = stamp(path);
    drop(file);
    let observed = observed?;
    if let Err(error) = written.and_then(|()| sync_parent(path)) {
        remove_known(path, &observed)
            .map_err(|error| PersistenceError::CleanupRequired(Box::new(error)))?;
        return Err(PersistenceError::Io(error));
    }
    Ok(observed)
}

pub(super) fn remove_known(path: &Path, expected: &Stamp) -> Result<(), PersistenceError> {
    require(path, expected)?;
    fs::remove_file(path)?;
    sync_parent(path)?;
    Ok(())
}

pub(super) fn replace(
    stage: &Path,
    target: &Path,
    stage_stamp: &Stamp,
    target_stamp: &Stamp,
) -> Result<(), PersistenceError> {
    require(stage, stage_stamp)?;
    require(target, target_stamp)?;
    super::super::replace_file(stage, target).map_err(|error| match error {
        super::super::ConfigStoreError::Io(error) => PersistenceError::Io(error),
        _ => PersistenceError::Conflict,
    })?;
    sync_parent(target)?;
    require(target, stage_stamp)
}

#[cfg(unix)]
fn restricted_create(path: &Path, permissions_from: &Path) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // stage 不需要其他用户读取；仅保留原文件 owner 权限的交集，绝不增加组/其他权限。
    let mode = fs::metadata(permissions_from)?.mode() & 0o600;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    if file.metadata()?.uid() != fs::metadata(permissions_from)?.uid() {
        return Err(io::Error::other(
            "configuration owner differs from process owner",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn permission_stamp(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path)?;
    Ok(format!(
        "{}:{}:{}",
        metadata.uid(),
        metadata.gid(),
        metadata.mode()
    ))
}

#[cfg(windows)]
fn permission_stamp(path: &Path) -> io::Result<String> {
    let descriptor = protected_descriptor(path)?;
    let bytes = descriptor
        .iter()
        .flat_map(|word| word.to_ne_bytes())
        .collect::<Vec<_>>();
    Ok(format!(
        "{}:{}",
        fs::metadata(path)?.permissions().readonly(),
        super::sha256_digest(&bytes)
    ))
}

#[cfg(windows)]
fn restricted_create(path: &Path, permissions_from: &Path) -> io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };

    let mut descriptor = protected_descriptor(permissions_from)?;
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

#[cfg(all(test, windows))]
pub(super) fn access_entries(path: &Path) -> Vec<Vec<u8>> {
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
                // INHERITED_ACE 仅描述来源；测试比较 ACE 顺序、主体、权限和有效继承限制。
                bytes[1] &= !(INHERITED_ACE as u8);
                bytes
            })
            .collect()
    }
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(
        path.parent()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
    )?
    .sync_all()
}

#[cfg(windows)]
fn sync_parent(_: &Path) -> io::Result<()> {
    // 每个文件先 FlushFileBuffers，替换复用 MoveFileExW WRITE_THROUGH；
    // Windows 不冒充已执行 Unix 的目录 fsync，也不承诺硬件断电恢复。
    Ok(())
}
