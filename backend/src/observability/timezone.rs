//! 日志与管理面共用的进程展示时区，不影响 UTC 存储和统计日。

use std::ffi::OsStr;
use std::sync::OnceLock;

use jiff::tz::{TimeZone, TimeZoneDatabase};

pub(crate) struct DisplayTimezone {
    pub(crate) zone: TimeZone,
    pub(crate) name: String,
    pub(crate) fallback_reason: Option<&'static str>,
}

/// 启动阶段冻结一次；运行中修改 TZ 或系统时区需要重启，避免日志和页面漂移。
pub(crate) fn display_timezone() -> &'static DisplayTimezone {
    static TIMEZONE: OnceLock<DisplayTimezone> = OnceLock::new();
    TIMEZONE.get_or_init(|| {
        let tz = std::env::var_os("TZ");
        resolve(
            tz.as_deref(),
            |name| lookup_timezone(name, jiff::tz::db()),
            read_openwrt_timezone,
            || TimeZone::try_system().ok(),
        )
    })
}

/// 系统数据库优先；精简固件没有数据库或缺少单个条目时，补查内嵌规则。
fn lookup_timezone(name: &str, database: &TimeZoneDatabase) -> Option<TimeZone> {
    database
        .get(name)
        .or_else(|_| TimeZoneDatabase::bundled().get(name))
        .ok()
}

/// 仅在 OpenWrt 上读取 UCI，不让普通 Linux 依赖 uci，也不通过 shell 解释配置。
fn read_openwrt_timezone() -> Result<Option<String>, &'static str> {
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/etc/openwrt_release").exists() {
        return read_uci_zonename(|| {
            std::process::Command::new("/sbin/uci")
                .args(["-q", "get", "system.@system[0].zonename"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
        });
    }
    Ok(None)
}

/// 命令和解码失败只保留安全原因码，不能公开配置原值或 UCI 的错误输出。
#[cfg(any(target_os = "linux", test))]
fn read_uci_zonename(
    run: impl FnOnce() -> std::io::Result<std::process::Output>,
) -> Result<Option<String>, &'static str> {
    let output = run().map_err(|_| "system_timezone_unavailable")?;
    if !output.status.success() {
        return Err("system_timezone_unavailable");
    }
    String::from_utf8(output.stdout)
        .map(Some)
        .map_err(|_| "invalid_tz_encoding")
}

/// 显式 TZ 不回退 UCI 或系统时区；只公开固定原因码，不传播配置值或底层错误。
fn resolve(
    tz: Option<&OsStr>,
    lookup: impl FnOnce(&str) -> Option<TimeZone>,
    openwrt: impl FnOnce() -> Result<Option<String>, &'static str>,
    system: impl FnOnce() -> Option<TimeZone>,
) -> DisplayTimezone {
    let zone = match tz {
        Some(value) => {
            let Some(value) = value.to_str() else {
                return fallback("invalid_tz_encoding");
            };
            // 空 TZ 遵循系统惯例视为 UTC；冒号和 zoneinfo 路径只提取数据库标识，
            // 不读取任意环境指定的文件，也不把路径作为 API 名称返回。
            let value = value.strip_prefix(':').unwrap_or(value);
            if value.is_empty() {
                return named(TimeZone::UTC);
            }
            let name = value
                .split_once("zoneinfo/")
                .map_or(value, |(_, name)| name);
            match lookup(name) {
                Some(zone) => zone,
                None => return fallback("invalid_tz"),
            }
        }
        None => match openwrt() {
            Ok(Some(name)) => {
                // UCI 输出带行尾；空名称不是 UTC，不能沿用显式空 TZ 的特殊语义。
                let name = name.trim();
                if name.is_empty() {
                    return fallback("invalid_tz");
                }
                match lookup(name) {
                    Some(zone) => zone,
                    None => return fallback("invalid_tz"),
                }
            }
            Ok(None) => match system() {
                Some(zone) => zone,
                None => return fallback("system_timezone_unavailable"),
            },
            Err(reason) => return fallback(reason),
        },
    };
    named(zone)
}

/// 无名称 TZif、POSIX 规则和固定偏移不能作为浏览器 IANA 标识，统一显式降级。
fn named(zone: TimeZone) -> DisplayTimezone {
    let Some(name) = zone.iana_name() else {
        return fallback("unsupported_timezone");
    };
    DisplayTimezone {
        name: name.to_owned(),
        zone,
        fallback_reason: None,
    }
}

fn fallback(reason: &'static str) -> DisplayTimezone {
    DisplayTimezone {
        zone: TimeZone::UTC,
        name: "UTC".to_owned(),
        fallback_reason: Some(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_tz_has_priority_and_never_exposes_invalid_values() {
        for (input, expected, reason) in [
            ("UTC", "UTC", None),
            ("", "UTC", None),
            ("Asia/Shanghai", "Asia/Shanghai", None),
            ("asia/SHANGHAI", "Asia/Shanghai", None),
            (":Asia/Shanghai", "Asia/Shanghai", None),
            ("/usr/share/zoneinfo/Asia/Shanghai", "Asia/Shanghai", None),
            (":/usr/share/zoneinfo/Asia/Shanghai", "Asia/Shanghai", None),
            ("secret-invalid-value", "UTC", Some("invalid_tz")),
            ("EST5EDT,M3.2.0,M11.1.0", "UTC", Some("invalid_tz")),
        ] {
            let result = resolve(
                Some(OsStr::new(input)),
                |name| TimeZone::get(name).ok(),
                || panic!("显式 TZ 不能读取 UCI"),
                || panic!("TZ 优先于系统"),
            );
            assert_eq!(result.name, expected, "{input}");
            assert_eq!(result.zone.iana_name(), Some(expected));
            assert_eq!(result.fallback_reason, reason);
        }
    }

    #[test]
    fn explicit_alias_accepts_the_name_returned_by_the_database() {
        // 系统库和内嵌库可能返回不同名称；受控查询验证不强制保留输入别名。
        let canonical = TimeZone::get("America/New_York").unwrap();
        let result = resolve(
            Some(OsStr::new("US/Eastern")),
            |name| {
                assert_eq!(name, "US/Eastern");
                Some(canonical.clone())
            },
            || panic!("显式 TZ 不能读取 UCI"),
            || panic!("显式 TZ 不能回退系统时区"),
        );
        assert_eq!(result.name, "America/New_York");
        assert_eq!(result.zone, canonical);
        assert_eq!(result.fallback_reason, None);
    }

    #[test]
    fn missing_legacy_alias_returns_explicit_utc_without_system_fallback() {
        // 受控模拟所有规则来源均无法解析名称，不修改全局 TZ/TZDIR。
        let result = resolve(
            Some(OsStr::new("US/Eastern")),
            |name| {
                assert_eq!(name, "US/Eastern");
                None
            },
            || panic!("显式 TZ 不能读取 UCI"),
            || panic!("缺失别名不能被系统时区掩盖"),
        );
        assert_eq!(result.name, "UTC");
        assert_eq!(result.zone, TimeZone::UTC);
        assert_eq!(result.fallback_reason, Some("invalid_tz"));
    }

    #[test]
    fn system_timezone_requires_a_browser_iana_name() {
        let result = resolve(
            None,
            |_| panic!("未设置 TZ 不查询名称"),
            || Ok(None),
            || TimeZone::get("Asia/Shanghai").ok(),
        );
        assert_eq!(result.name, "Asia/Shanghai");
        assert_eq!(result.fallback_reason, None);
        let result = resolve(
            None,
            |_| panic!("未设置 TZ 不查询名称"),
            || Ok(None),
            || Some(TimeZone::UTC),
        );
        assert_eq!(result.name, "UTC");
        assert_eq!(result.fallback_reason, None);
        let result = resolve(
            None,
            |_| panic!("未设置 TZ 不查询名称"),
            || Ok(None),
            || None,
        );
        assert_eq!(result.name, "UTC");
        assert_eq!(result.fallback_reason, Some("system_timezone_unavailable"));
        for zone in [
            TimeZone::fixed(jiff::tz::Offset::from_hours(8).unwrap()),
            TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap(),
        ] {
            let result = resolve(
                None,
                |_| panic!("未设置 TZ 不查询名称"),
                || Ok(None),
                || Some(zone),
            );
            assert_eq!(result.name, "UTC");
            assert_eq!(result.zone.iana_name(), Some("UTC"));
            assert_eq!(result.fallback_reason, Some("unsupported_timezone"));
        }
    }

    #[test]
    fn openwrt_zonename_uses_bundled_rules_without_system_zoneinfo() {
        let missing_database = TimeZoneDatabase::none();
        assert!(missing_database.get("Asia/Shanghai").is_err());
        let result = resolve(
            None,
            |name| lookup_timezone(name, &missing_database),
            || Ok(Some("Asia/Shanghai\n".to_owned())),
            || panic!("UCI 名称不依赖 /etc/localtime"),
        );
        assert_eq!(result.name, "Asia/Shanghai");
        assert_eq!(result.fallback_reason, None);
        assert_eq!(
            super::super::format_log_time_in_zone(std::time::SystemTime::UNIX_EPOCH, result.zone,)
                .unwrap(),
            "1970-01-01T08:00:00.000+08:00"
        );
    }

    #[test]
    fn explicit_tz_also_uses_bundled_rules_without_system_zoneinfo() {
        let result = resolve(
            Some(OsStr::new("Asia/Shanghai")),
            |name| lookup_timezone(name, &TimeZoneDatabase::none()),
            || panic!("显式 TZ 不能读取 UCI"),
            || panic!("显式 TZ 不能读取系统时区"),
        );
        assert_eq!(result.name, "Asia/Shanghai");
        assert_eq!(result.fallback_reason, None);
    }

    #[test]
    fn invalid_openwrt_zonename_never_falls_back_to_another_source() {
        for name in ["", " \n", "secret-invalid-value", "CST-8", ":Asia/Shanghai"] {
            let result = resolve(
                None,
                |name| lookup_timezone(name, &TimeZoneDatabase::none()),
                || Ok(Some(name.to_owned())),
                || panic!("无效 UCI 名称不能被系统时区掩盖"),
            );
            assert_eq!(result.name, "UTC", "{name:?}");
            assert_eq!(result.zone, TimeZone::UTC);
            assert_eq!(result.fallback_reason, Some("invalid_tz"));
        }
        let unavailable = resolve(
            None,
            |_| None,
            || Ok(Some("Asia/Shanghai".to_owned())),
            || panic!("规则不可用必须明确降级"),
        );
        assert_eq!(unavailable.fallback_reason, Some("invalid_tz"));
    }

    #[test]
    fn openwrt_read_failure_keeps_the_safe_reason_and_never_probes_system() {
        for reason in ["system_timezone_unavailable", "invalid_tz_encoding"] {
            let result = resolve(
                None,
                |_| panic!("UCI 读取失败不查询名称"),
                || Err(reason),
                || panic!("UCI 读取失败不能被系统时区掩盖"),
            );
            assert_eq!(result.name, "UTC");
            assert_eq!(result.zone, TimeZone::UTC);
            assert_eq!(result.fallback_reason, Some(reason));
        }
    }

    #[test]
    fn uci_output_classifies_io_exit_and_encoding_failures() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt as _;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt as _;

        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
        ] {
            assert_eq!(
                read_uci_zonename(|| Err(std::io::Error::from(kind))),
                Err("system_timezone_unavailable")
            );
        }
        for (status, stdout, expected) in [
            (
                0,
                b"Asia/Shanghai\n".to_vec(),
                Ok(Some("Asia/Shanghai\n".to_owned())),
            ),
            (0, Vec::new(), Ok(Some(String::new()))),
            (
                1,
                b"private-config-value".to_vec(),
                Err("system_timezone_unavailable"),
            ),
            (0, vec![0xff], Err("invalid_tz_encoding")),
        ] {
            assert_eq!(
                read_uci_zonename(|| Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(status),
                    stdout,
                    stderr: b"private-command-error".to_vec(),
                })),
                expected
            );
        }
    }

    #[test]
    fn startup_warns_once_without_exposing_invalid_timezone() {
        let output = {
            let _child_process = crate::test_support::child_process_window();
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "observability::timezone::tests::timezone_warning_worker",
                    "--nocapture",
                ])
                .env("FLUXDNS_TIMEZONE_WARNING_TEST", "1")
                .env("TZ", "PRIVATE_TIMEZONE_VALUE")
                .output()
                .unwrap()
        };
        assert!(output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.matches("timezone_fallback").count(), 1, "{stderr}");
        assert!(stderr.contains("invalid_tz"), "{stderr}");
        assert!(!stderr.contains("PRIVATE_TIMEZONE_VALUE"), "{stderr}");
    }

    #[test]
    fn timezone_warning_worker() {
        if std::env::var_os("FLUXDNS_TIMEZONE_WARNING_TEST").is_none() {
            return;
        }
        super::super::init_bootstrap().unwrap();
        assert_eq!(display_timezone().fallback_reason, Some("invalid_tz"));
        let metrics = crate::management::MetricsOwner::new().process_metrics();
        assert_eq!(metrics.timezone, "UTC");
        assert_eq!(
            metrics.timezone_fallback_reason.as_deref(),
            Some("invalid_tz")
        );
        assert_eq!(
            super::super::format_log_time(std::time::SystemTime::UNIX_EPOCH).unwrap(),
            "1970-01-01T00:00:00.000+00:00"
        );
    }

    #[test]
    fn process_timezone_is_frozen_and_shared() {
        assert!(std::ptr::eq(display_timezone(), display_timezone()));
        let metrics = crate::management::MetricsOwner::new().process_metrics();
        assert_eq!(metrics.timezone, display_timezone().name);
        assert_eq!(
            metrics.timezone_fallback_reason.as_deref(),
            display_timezone().fallback_reason
        );
        let expected = jiff::Timestamp::UNIX_EPOCH
            .to_zoned(TimeZone::get(&metrics.timezone).unwrap())
            .strftime("%Y-%m-%dT%H:%M:%S%.3f%:z")
            .to_string();
        assert_eq!(
            super::super::format_log_time(std::time::SystemTime::UNIX_EPOCH).unwrap(),
            expected
        );
    }
}
