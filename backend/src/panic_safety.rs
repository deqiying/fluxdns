//! panic 输出只包含受控定位信息，不改变 unwind、task owner 或进程退出策略。

use std::backtrace::{Backtrace, BacktraceStatus};
use std::io::Write;

/// 在日志和业务初始化前安装；不调用 telemetry，避免 panic 路径递归或等待 writer。
pub(crate) fn install() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = match Backtrace::capture().status() {
            BacktraceStatus::Captured => "available",
            BacktraceStatus::Disabled => "disabled",
            _ => "unsupported",
        };
        let mut stderr = std::io::stderr().lock();
        if let Some(location) = info.location() {
            let file = safe_source_file(location.file());
            // stderr 自身失败时没有可靠的备用输出；不得因此在 hook 内再次 panic。
            let _ = writeln!(
                stderr,
                "FluxDNS panic: category=panic file={file} line={} column={} backtrace={backtrace}",
                location.line(),
                location.column(),
            );
        } else {
            let _ = writeln!(
                stderr,
                "FluxDNS panic: category=panic location=unknown backtrace={backtrace}"
            );
        }
    }));
}

fn safe_source_file(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or("");
    if !file.is_empty()
        && file.len() <= 128
        && file
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        file
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_location_does_not_expose_build_paths_or_control_characters() {
        assert_eq!(
            safe_source_file("D:\\private\\source\\worker.rs"),
            "worker.rs"
        );
        assert_eq!(safe_source_file("/home/private/worker.rs"), "worker.rs");
        assert_eq!(safe_source_file("worker\nsecret.rs"), "unknown");
    }

    #[test]
    fn subprocess_panic_probe() {
        let Ok(mode) = std::env::var("FLUXDNS_PANIC_PROBE") else {
            return;
        };
        install();
        if mode == "worker" {
            let result = std::thread::Builder::new()
                .name("PRIVATE_THREAD_MARKER".to_owned())
                .spawn(|| panic!("PRIVATE_PAYLOAD_MARKER"))
                .unwrap()
                .join();
            assert!(result.is_err(), "hook must not turn panic into success");
        } else {
            panic!("PRIVATE_PAYLOAD_MARKER");
        }
    }

    #[test]
    fn real_panic_output_is_redacted_and_unwind_is_preserved() {
        for (mode, success) in [("main", false), ("worker", true)] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "panic_safety::tests::subprocess_panic_probe",
                    "--nocapture",
                ])
                .env("FLUXDNS_PANIC_PROBE", mode)
                .env("RUST_BACKTRACE", "1")
                .output()
                .unwrap();
            assert_eq!(output.status.success(), success);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stderr.contains("category=panic file=panic_safety.rs line="));
            assert!(stderr.contains("backtrace=available"));
            for marker in [
                "PRIVATE_PAYLOAD_MARKER",
                "PRIVATE_THREAD_MARKER",
                "stack backtrace:",
            ] {
                assert!(!stderr.contains(marker));
                assert!(!stdout.contains(marker));
            }
        }
    }
}
