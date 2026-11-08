//! 资源错误的安全投影；只格式化受控枚举和数字，不读取任意错误的 Display。

use crate::diagnostics::Diagnostic;
use crate::ports::PortErrorClass;

use super::{
    HostsParseError, LocalResourceRefreshWorkerError, RemoteResourceError,
    ResourceFetcherBuildError, ResourceLoadError, ResourceRefreshWorkerError,
    RuleResourceLoadError,
};

impl ResourceFetcherBuildError {
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        let (code, message) = match self {
            Self::InvalidOutbound { .. } => (
                "RESOURCE_INVALID_OUTBOUND",
                "资源下载出口或代理凭据无法准备",
            ),
            Self::DuplicateOutbound { .. } => {
                ("RESOURCE_DUPLICATE_OUTBOUND", "资源下载出口标识重复")
            }
            Self::Client => ("RESOURCE_CLIENT_BUILD", "资源 HTTP 客户端构造失败"),
            Self::Proxy => ("RESOURCE_PROXY_BUILD", "资源代理客户端构造失败"),
            Self::ValidatorScope => ("RESOURCE_VALIDATOR_SCOPE", "资源条件请求作用域初始化失败"),
        };
        Diagnostic::new("resource_fetcher_build", code, message)
    }
}

impl RemoteResourceError {
    /// 不返回缓存路径、URL、代理凭据、响应正文或任意底层错误文本。
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        let (stage, code, message) = match self {
            Self::UnsupportedSource { .. } => (
                "resource_source",
                "RESOURCE_UNSUPPORTED_SOURCE",
                "资源来源类型不受支持".to_owned(),
            ),
            Self::UnsupportedScheme => (
                "resource_source",
                "RESOURCE_UNSUPPORTED_SCHEME",
                "远程资源仅支持 HTTP 或 HTTPS".to_owned(),
            ),
            Self::MissingHost => (
                "resource_source",
                "RESOURCE_MISSING_HOST",
                "远程资源缺少主机名".to_owned(),
            ),
            Self::UnsafeUrl => (
                "resource_source",
                "RESOURCE_UNSAFE_URL",
                "远程资源地址包含不允许的凭据或片段".to_owned(),
            ),
            Self::ZeroMaxBytes => (
                "resource_source",
                "RESOURCE_ZERO_MAX_BYTES",
                "远程资源字节上限必须大于零".to_owned(),
            ),
            Self::InvalidPersistencePaths => (
                "resource_persist",
                "RESOURCE_INVALID_PERSISTENCE_PATHS",
                "资源持久化目标必须是不同文件".to_owned(),
            ),
            Self::Cancelled { .. } => (
                "resource_fetch",
                "RESOURCE_CANCELLED",
                "远程资源准备已取消".to_owned(),
            ),
            Self::DeadlineExceeded => (
                "resource_fetch",
                "RESOURCE_TIMEOUT",
                "远程资源准备超过时限".to_owned(),
            ),
            Self::Fetch(error) => {
                if let Some(diagnostic) = error.diagnostic() {
                    return diagnostic.clone();
                }
                let stage = match error.operation() {
                    "resource_fetch.read" => "resource_read",
                    "resource_fetch.validator" | "resource_validators.validate" => {
                        "resource_validate"
                    }
                    _ => "resource_fetch",
                };
                let (code, message) = match error.class() {
                    PortErrorClass::Timeout => ("RESOURCE_TIMEOUT", "远程资源请求超时"),
                    PortErrorClass::Cancelled(_) => ("RESOURCE_CANCELLED", "远程资源请求已取消"),
                    PortErrorClass::Unavailable => {
                        ("RESOURCE_CONNECT_TLS", "远程资源连接、TLS 或读取失败")
                    }
                    PortErrorClass::PermissionDenied => {
                        ("RESOURCE_PERMISSION_DENIED", "远程资源访问被拒绝")
                    }
                    PortErrorClass::ResourceExhausted => {
                        ("RESOURCE_TOO_LARGE", "远程资源超过字节上限")
                    }
                    PortErrorClass::ProtocolViolation => {
                        ("RESOURCE_PROTOCOL", "远程资源响应不符合协议")
                    }
                    PortErrorClass::InvalidInput => {
                        ("RESOURCE_INVALID_REQUEST", "远程资源请求参数无效")
                    }
                    PortErrorClass::CorruptData => ("RESOURCE_CORRUPT_DATA", "远程资源数据损坏"),
                    PortErrorClass::Internal => ("RESOURCE_FETCH_FAILED", "远程资源请求失败"),
                };
                return Diagnostic::new(stage, code, message);
            }
            Self::TooLarge { actual, max } => (
                "resource_read",
                "RESOURCE_TOO_LARGE",
                format!("资源超过字节上限：实际字节={actual}，上限字节={max}"),
            ),
            Self::InvalidUtf8 => (
                "resource_parse",
                "RESOURCE_INVALID_UTF8",
                "资源内容不是有效 UTF-8".to_owned(),
            ),
            Self::UnsupportedFormat => (
                "resource_parse",
                "RESOURCE_UNSUPPORTED_FORMAT",
                "远程规则格式不受支持".to_owned(),
            ),
            Self::Parse { source, limits } => return source.diagnostic(*limits),
            Self::Persistence { operation, source } => {
                let stage = match *operation {
                    "content_read" | "manifest_read" => "resource_restore",
                    _ => "resource_persist",
                };
                return Diagnostic::new(
                    stage,
                    "RESOURCE_IO",
                    format!("资源本地存储操作失败；I/O 类别={:?}", source.kind()),
                );
            }
            Self::ManifestSerialization => (
                "resource_persist",
                "RESOURCE_MANIFEST_SERIALIZE",
                "资源清单序列化失败".to_owned(),
            ),
            Self::ManifestInvalid => (
                "resource_restore",
                "RESOURCE_MANIFEST_INVALID",
                "资源本地清单无效".to_owned(),
            ),
            Self::ManifestMismatch => (
                "resource_restore",
                "RESOURCE_MANIFEST_MISMATCH",
                "资源本地清单与内容不匹配".to_owned(),
            ),
            Self::UnexpectedNotModified => (
                "resource_validate",
                "RESOURCE_UNEXPECTED_304",
                "远程资源返回 HTTP 状态码 304，但没有匹配的已验证内容".to_owned(),
            ),
        };
        Diagnostic::new(stage, code, message)
    }
}

impl RuleResourceLoadError {
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        let (resource, code, message) = match self {
            Self::Parse {
                resource,
                source,
                limits,
            } => return source.diagnostic(**limits).with_resource(resource),
            Self::UnsupportedFormat { resource } => (
                resource,
                "RESOURCE_UNSUPPORTED_FORMAT",
                "规则格式不受支持".to_owned(),
            ),
            Self::UnsupportedSource { resource, .. } => (
                resource,
                "RESOURCE_UNSUPPORTED_SOURCE",
                "规则来源类型不受支持".to_owned(),
            ),
            Self::Symlink { resource } => (
                resource,
                "RESOURCE_SYMLINK",
                "本地规则资源不允许使用符号链接".to_owned(),
            ),
            Self::Metadata { resource, kind } => (
                resource,
                "RESOURCE_METADATA_IO",
                format!("本地规则资源检查失败；I/O 类别={kind:?}"),
            ),
            Self::Read { resource, kind } => (
                resource,
                "RESOURCE_READ_IO",
                format!("本地规则资源读取失败；I/O 类别={kind:?}"),
            ),
            Self::UnstableFile { resource } => (
                resource,
                "RESOURCE_UNSTABLE_FILE",
                "本地规则资源在读取期间发生变化".to_owned(),
            ),
            Self::InvalidUtf8 { resource } => (
                resource,
                "RESOURCE_INVALID_UTF8",
                "本地规则内容不是有效 UTF-8".to_owned(),
            ),
            Self::TooLarge { resource, max } => (
                resource,
                "RESOURCE_TOO_LARGE",
                format!("本地规则资源超过字节上限；work.rule_set_max_size_bytes={max}"),
            ),
        };
        Diagnostic::new("resource_load", code, message).with_resource(resource)
    }
}

impl ResourceLoadError {
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        let (resource, code, message) = match self {
            Self::Parse {
                resource,
                source,
                limits,
            } => {
                let (code, message, line) = match source {
                    HostsParseError::InputTooLarge => {
                        ("HOSTS_INPUT_TOO_LARGE", "Hosts 输入超过字节上限", None)
                    }
                    HostsParseError::TooManyRecords => {
                        ("HOSTS_TOO_MANY_RECORDS", "Hosts 记录数量超过上限", None)
                    }
                    HostsParseError::TooManyNames => {
                        ("HOSTS_TOO_MANY_NAMES", "Hosts 名称数量超过上限", None)
                    }
                    HostsParseError::LineTooLong { line } => {
                        ("HOSTS_LINE_TOO_LONG", "Hosts 行超过字节上限", Some(*line))
                    }
                    HostsParseError::MissingAddress { line } => {
                        ("HOSTS_MISSING_ADDRESS", "Hosts 缺少地址", Some(*line))
                    }
                    HostsParseError::InvalidAddress { line } => {
                        ("HOSTS_INVALID_ADDRESS", "Hosts 地址无效", Some(*line))
                    }
                    HostsParseError::MissingName { line } => {
                        ("HOSTS_MISSING_NAME", "Hosts 缺少名称", Some(*line))
                    }
                    HostsParseError::InvalidNameAtLine { line } => {
                        ("HOSTS_INVALID_NAME", "Hosts 名称无效", Some(*line))
                    }
                    HostsParseError::ConflictingCname => {
                        ("HOSTS_CONFLICTING_CNAME", "Hosts CNAME 记录冲突", None)
                    }
                    HostsParseError::InvalidName => ("HOSTS_INVALID_NAME", "Hosts 名称无效", None),
                    HostsParseError::InvalidJson => {
                        ("HOSTS_INVALID_JSON", "Hosts JSON 格式无效", None)
                    }
                    HostsParseError::UnsupportedRecordType => (
                        "HOSTS_UNSUPPORTED_RECORD_TYPE",
                        "Hosts 记录类型不受支持",
                        None,
                    ),
                    HostsParseError::ConflictingRecords => {
                        ("HOSTS_CONFLICTING_RECORDS", "Hosts 记录类型冲突", None)
                    }
                };
                let position = line
                    .map(|line| format!("；行号={line}"))
                    .unwrap_or_default();
                return Diagnostic::new("resource_parse", code, format!(
                    "{message}{position}；当前限制：输入字节={}，记录数量={}，名称数量={}，行字节={}",
                    limits.max_input_bytes, limits.max_records, limits.max_names, limits.max_line_bytes,
                )).with_resource(resource);
            }
            Self::UnsupportedFormat { resource } => (
                resource,
                "RESOURCE_UNSUPPORTED_FORMAT",
                "Hosts 格式不受支持".to_owned(),
            ),
            Self::Symlink { resource } => (
                resource,
                "RESOURCE_SYMLINK",
                "本地 Hosts 资源不允许使用符号链接".to_owned(),
            ),
            Self::Metadata { resource, kind } => (
                resource,
                "RESOURCE_METADATA_IO",
                format!("本地 Hosts 资源检查失败；I/O 类别={kind:?}"),
            ),
            Self::Read { resource, kind } => (
                resource,
                "RESOURCE_READ_IO",
                format!("本地 Hosts 资源读取失败；I/O 类别={kind:?}"),
            ),
            Self::UnstableFile { resource } => (
                resource,
                "RESOURCE_UNSTABLE_FILE",
                "本地 Hosts 资源在读取期间发生变化".to_owned(),
            ),
            Self::InvalidUtf8 { resource } => (
                resource,
                "RESOURCE_INVALID_UTF8",
                "本地 Hosts 内容不是有效 UTF-8".to_owned(),
            ),
            Self::TooLarge { resource, max } => (
                resource,
                "RESOURCE_TOO_LARGE",
                format!("本地 Hosts 资源超过字节上限；上限字节={max}"),
            ),
        };
        Diagnostic::new("resource_load", code, message).with_resource(resource)
    }
}

impl ResourceRefreshWorkerError {
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        let (stage, code, message) = match self {
            Self::Fetch(source) => return source.diagnostic(),
            Self::UnsupportedSource { .. } => (
                "resource_refresh",
                "RESOURCE_UNSUPPORTED_SOURCE",
                "远程资源刷新来源不受支持",
            ),
            Self::Begin(_) => (
                "resource_refresh_begin",
                "RESOURCE_REFRESH_BEGIN",
                "远程资源刷新无法取得执行许可",
            ),
            Self::Publish(_) => (
                "resource_refresh_publish",
                "RESOURCE_REFRESH_PUBLISH",
                "远程资源刷新快照发布失败",
            ),
            Self::Release(_) => (
                "resource_refresh_release",
                "RESOURCE_REFRESH_RELEASE",
                "远程资源刷新执行许可释放失败",
            ),
        };
        Diagnostic::new(stage, code, message)
    }
}

impl LocalResourceRefreshWorkerError {
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        let (stage, code, message) = match self {
            Self::HostsLoad(source) => return source.diagnostic(),
            Self::RuleSetLoad(source) => return source.diagnostic(),
            Self::UnsupportedSource { .. } => (
                "resource_refresh",
                "RESOURCE_UNSUPPORTED_SOURCE",
                "本地资源刷新来源不受支持",
            ),
            Self::Begin(_) => (
                "resource_refresh_begin",
                "RESOURCE_REFRESH_BEGIN",
                "本地资源刷新无法取得执行许可",
            ),
            Self::Publish(_) => (
                "resource_refresh_publish",
                "RESOURCE_REFRESH_PUBLISH",
                "本地资源刷新快照发布失败",
            ),
            Self::Release(_) => (
                "resource_refresh_release",
                "RESOURCE_REFRESH_RELEASE",
                "本地资源刷新执行许可释放失败",
            ),
        };
        Diagnostic::new(stage, code, message)
    }
}
