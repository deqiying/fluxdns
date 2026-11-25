use std::fs;
use std::path::PathBuf;

use super::*;

/// 每个测试一个 `_fluxdns/p2-config-disk/<随机>` 目录，结束时删除。
struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let name = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../_fluxdns/p2-config-disk")
            .join(name);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn read_classifies_text_missing_and_unreadable() {
    let root = Root::new();
    let path = root.0.join("config.yaml");
    assert_eq!(observe(&path), DiskObservation::Missing);
    fs::write(&path, "version: 2\n").unwrap();
    assert_eq!(
        observe(&path),
        DiskObservation::Readable {
            digest: digest(b"version: 2\n")
        }
    );
    fs::write(&path, [0xff, 0xfe]).unwrap();
    assert_eq!(observe(&path), DiskObservation::Unreadable);
    fs::write(&path, vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap();
    assert_eq!(observe(&path), DiskObservation::Unreadable);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(observe(&path), DiskObservation::Unreadable);
}

#[test]
fn status_compares_digest_only() {
    let text = digest(b"a");
    let observation = DiskObservation::Readable {
        digest: text.clone(),
    };
    assert_eq!(observation.status(&text), DiskStatus::InSync);
    assert!(observation.matches(&text));
    assert_eq!(observation.status(&digest(b"b")), DiskStatus::Modified);
    assert_eq!(DiskObservation::Missing.status(&text), DiskStatus::Missing);
    assert_eq!(
        DiskObservation::Unreadable.status(&text),
        DiskStatus::Unreadable
    );
    assert_eq!(DiskStatus::Modified.as_str(), "modified");
}

#[test]
fn write_atomic_creates_replaces_and_reuses_stale_temp() {
    let root = Root::new();
    let path = root.0.join("config.yaml");
    write_atomic(&path, b"first\n").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"first\n");
    // 上次崩溃残留的固定名临时文件会被截断复用，不影响正确性。
    fs::write(temp_path(&path), b"stale content that is longer").unwrap();
    write_atomic(&path, b"second\r\n").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"second\r\n");
    assert!(!temp_path(&path).exists());
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}

#[test]
fn write_atomic_rejects_read_only_target_without_changing_it() {
    let root = Root::new();
    let path = root.0.join("config.yaml");
    fs::write(&path, b"original\n").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions.clone()).unwrap();
    let error = write_atomic(&path, b"changed\n").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(fs::read(&path).unwrap(), b"original\n");
    assert!(!temp_path(&path).exists());
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).unwrap();
}

#[cfg(unix)]
#[test]
fn write_atomic_preserves_unix_mode() {
    use std::os::unix::fs::PermissionsExt;
    let root = Root::new();
    let path = root.0.join("config.yaml");
    fs::write(&path, b"original\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    write_atomic(&path, b"changed\n").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o640
    );
}

#[cfg(windows)]
#[test]
fn write_atomic_preserves_windows_access_entries() {
    let root = Root::new();
    let path = root.0.join("config.yaml");
    fs::write(&path, b"original\n").unwrap();
    let before = access_entries(&path);
    write_atomic(&path, b"changed\n").unwrap();
    assert_eq!(access_entries(&path), before);
    assert_eq!(fs::read(&path).unwrap(), b"changed\n");
}

#[cfg(unix)]
#[test]
fn write_path_follows_symlink_and_keeps_link() {
    let root = Root::new();
    let real = root.0.join("real.yaml");
    let link = root.0.join("config.yaml");
    fs::write(&real, b"original\n").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let target = resolve_write_path(&link);
    write_atomic(&target, b"changed\n").unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&real).unwrap(), b"changed\n");
}

#[test]
fn legacy_sidecars_are_listed_but_temp_file_is_not() {
    let root = Root::new();
    let source = root.0.join("config.yaml");
    fs::write(&source, b"version: 2\n").unwrap();
    for name in [
        ".config.yaml.fluxdns-journal",
        ".config.yaml.fluxdns-lock",
        ".config.yaml.fluxdns-v2-stage",
        ".config.yaml.fluxdns-tmp",
        ".other.yaml.fluxdns-journal",
    ] {
        fs::write(root.0.join(name), b"x").unwrap();
    }
    let names = legacy_sidecars(&source)
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            ".config.yaml.fluxdns-journal",
            ".config.yaml.fluxdns-lock",
            ".config.yaml.fluxdns-v2-stage",
        ]
    );
    // 只提示，不删除。
    assert!(root.0.join(".config.yaml.fluxdns-journal").exists());
}

#[test]
fn io_diagnostic_does_not_echo_os_message() {
    let error = io::Error::new(io::ErrorKind::PermissionDenied, "C:/secret/path denied");
    let diagnostic = io_diagnostic("CONFIG_FILE_WRITE_FAILED", "写入失败", &error);
    assert_eq!(diagnostic.code, "CONFIG_FILE_WRITE_FAILED");
    assert!(diagnostic.message.contains("PermissionDenied"));
    assert!(!diagnostic.message.contains("secret"));
}
