//! 日志与管理面共用的进程展示时区，不影响 UTC 存储和统计日。

use std::ffi::OsStr;
use std::sync::OnceLock;

use jiff::tz::TimeZone;

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
        resolve(tz.as_deref(), || TimeZone::try_system().ok())
    })
}

/// TZ 显式存在时不回退系统时区；只公开固定原因码，不传播环境值或底层错误。
fn resolve(tz: Option<&OsStr>, system: impl FnOnce() -> Option<TimeZone>) -> DisplayTimezone {
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
            match TimeZone::get(name) {
                Ok(zone) => zone,
                Err(_) => return fallback("invalid_tz"),
            }
        }
        None => match system() {
            Some(zone) => zone,
            None => return fallback("system_timezone_unavailable"),
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
            ("US/Eastern", "US/Eastern", None),
            ("secret-invalid-value", "UTC", Some("invalid_tz")),
            ("EST5EDT,M3.2.0,M11.1.0", "UTC", Some("invalid_tz")),
        ] {
            let result = resolve(Some(OsStr::new(input)), || panic!("TZ 优先于系统"));
            assert_eq!(result.name, expected, "{input}");
            assert_eq!(result.zone.iana_name(), Some(expected));
            assert_eq!(result.fallback_reason, reason);
        }
    }

    #[test]
    fn system_timezone_requires_a_browser_iana_name() {
        let result = resolve(None, || TimeZone::get("Asia/Shanghai").ok());
        assert_eq!(result.name, "Asia/Shanghai");
        assert_eq!(result.fallback_reason, None);
        let result = resolve(None, || Some(TimeZone::UTC));
        assert_eq!(result.name, "UTC");
        assert_eq!(result.fallback_reason, None);
        let result = resolve(None, || None);
        assert_eq!(result.name, "UTC");
        assert_eq!(result.fallback_reason, Some("system_timezone_unavailable"));
        for zone in [
            TimeZone::fixed(jiff::tz::Offset::from_hours(8).unwrap()),
            TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap(),
        ] {
            let result = resolve(None, || Some(zone));
            assert_eq!(result.name, "UTC");
            assert_eq!(result.zone.iana_name(), Some("UTC"));
            assert_eq!(result.fallback_reason, Some("unsupported_timezone"));
        }
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
