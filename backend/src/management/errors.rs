//! 管理面的安全错误投影；稳定错误码保留兼容性，诊断只来自类型化错误和白名单字段。

use crate::config::change::EditError;
use crate::config::store::StoreError;
use crate::config::validate::{ConfigErrorKind, ConfigErrorReport};
use crate::diagnostics::Diagnostic;
use crate::service::{ControlError, ServiceReloadError};

use super::contract::{DiskState, ErrorCode, FieldError};

#[derive(Debug)]
pub(super) struct ManagementError {
    pub code: ErrorCode,
    // 诊断仅失败时分配，避免扩大每个 Result 的内联错误分支。
    pub diagnostic: Option<Box<Diagnostic>>,
    pub field_errors: Vec<FieldError>,
    /// 仅 `CONFIG_FILE_CHANGED` 携带的磁盘状态。
    pub disk: Option<DiskState>,
}

impl From<ErrorCode> for ManagementError {
    fn from(code: ErrorCode) -> Self {
        Self {
            code,
            diagnostic: None,
            field_errors: Vec::new(),
            disk: None,
        }
    }
}

impl ManagementError {
    fn with_diagnostic(code: ErrorCode, diagnostic: Diagnostic) -> Self {
        Self {
            diagnostic: Some(Box::new(diagnostic)),
            ..Self::from(code)
        }
    }
}

impl From<StoreError> for ManagementError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Busy => ErrorCode::OperationBusy.into(),
            StoreError::FileChanged(status) => Self {
                disk: Some(super::config_query::disk_state(status)),
                ..Self::from(ErrorCode::ConfigFileChanged)
            },
            StoreError::FileUnreadable(status) => Self::with_diagnostic(
                ErrorCode::ConfigFileUnreadable,
                match status {
                    crate::config::disk::DiskStatus::Missing => Diagnostic::new(
                        "configuration",
                        "CONFIG_FILE_MISSING",
                        "配置文件不存在，运行配置保持不变；可写入磁盘以恢复文件。",
                    ),
                    _ => Diagnostic::new(
                        "configuration",
                        "CONFIG_FILE_UNREADABLE",
                        "配置文件无法读取、超过 4 MiB 或不是 UTF-8 文本，运行配置保持不变。",
                    ),
                },
            ),
            StoreError::Edit(error) => {
                let field_errors = match &error {
                    EditError::Validation(report) => report
                        .errors
                        .iter()
                        .filter_map(|error| safe_config_path(&error.path))
                        .take(16)
                        .map(|path| FieldError {
                            path,
                            code: ErrorCode::ValidationFailed,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                Self {
                    field_errors,
                    ..Self::with_diagnostic(ErrorCode::ValidationFailed, edit_diagnostic(&error))
                }
            }
            StoreError::ConfirmationRequired => Self::with_diagnostic(
                ErrorCode::ConfirmationRequired,
                Diagnostic::new(
                    "configuration",
                    "RETENTION_SHORTENING_UNCONFIRMED",
                    "本次修改会缩短统计保留期，需要明确确认。",
                ),
            ),
            StoreError::Secret(error) => {
                Self::with_diagnostic(ErrorCode::ApplyFailed, secret_diagnostic(&error))
            }
            StoreError::Apply(diagnostic) => {
                Self::with_diagnostic(ErrorCode::ApplyFailed, diagnostic)
            }
            StoreError::Persist(diagnostic) => {
                Self::with_diagnostic(ErrorCode::PersistenceFailed, diagnostic)
            }
            StoreError::AlreadyInitialized => ErrorCode::SetupAlreadyCompleted.into(),
            StoreError::SetupRejected => ErrorCode::ValidationFailed.into(),
            StoreError::Internal => ErrorCode::InternalError.into(),
        }
    }
}

/// HTTP 默认文案覆盖全部稳定错误码，禁止退化为不含原因的通用英文。
pub(super) fn default_message(code: &ErrorCode) -> &'static str {
    match code {
        ErrorCode::HttpVersionNotSupported => "不支持此 HTTP 版本。",
        ErrorCode::UriTooLong => "请求地址超过长度上限。",
        ErrorCode::HeadersTooLarge => "请求头超过大小上限。",
        ErrorCode::RequestTimeout => "请求超时；写入操作请先刷新配置状态确认结果，再决定是否重试。",
        ErrorCode::InternalError => "管理服务内部错误，请凭请求 ID 检查服务日志。",
        ErrorCode::SetupAlreadyCompleted => "服务已经完成初始化，不能重复创建初始管理员。",
        ErrorCode::AuthInvalidCredentials => "用户名或密码不正确。",
        ErrorCode::OriginRejected => "请求来源不被允许，请核对 WebUI 的访问地址。",
        ErrorCode::ConfigConflict => "配置发生冲突，请刷新后检查当前配置。",
        ErrorCode::InvalidArgument => "请求参数或格式无效，请检查输入。",
        ErrorCode::ValidationFailed => "配置校验失败，请检查字段及资源引用。",
        ErrorCode::PayloadTooLarge => "请求内容超过大小上限。",
        ErrorCode::VersionUnsupported => "不支持此配置或协议版本。",
        ErrorCode::AuthRequired => "会话已失效或尚未登录，请重新登录。",
        ErrorCode::Forbidden => "当前会话没有执行此操作的权限。",
        ErrorCode::NotFound => "请求的资源不存在。",
        ErrorCode::ConfigFileChanged => {
            "配置文件已在磁盘上被修改，与当前运行配置不一致；请选择覆盖磁盘或重新加载。"
        }
        ErrorCode::ConfigFileUnreadable => "配置文件缺失或无法读取，运行配置保持不变。",
        ErrorCode::OperationBusy => "另一个配置写操作正在进行，请稍后重试。",
        ErrorCode::ConfirmationRequired => "此操作需要明确确认其影响。",
        ErrorCode::PersistenceFailed => {
            "配置文件写入失败，请检查文件权限、只读属性和磁盘空间后重试。"
        }
        ErrorCode::ApplyFailed => "配置未能应用，原运行状态保持不变。",
        ErrorCode::CursorExpired => "查询游标已过期，请重新开始查询。",
        ErrorCode::RateLimited => "请求过于频繁，请稍后重试。",
        ErrorCode::ServiceUnavailable => "管理服务暂不可用，请检查服务状态后重试。",
    }
}

/// 凭据校验仅公开来源类别及代理标识，不公开引用位置、凭据值或 URL。
pub(super) fn secret_diagnostic(
    error: &crate::config::resolve::SecretValidationError,
) -> Diagnostic {
    use crate::config::resolve::{SecretResolveError, SecretValidationError};
    match error {
        SecretValidationError::Resolve { outbound, source } => {
            let (code, message) = match source {
                SecretResolveError::InvalidReference => {
                    ("SECRET_REFERENCE_INVALID", "代理凭据引用无效。".to_owned())
                }
                SecretResolveError::Missing { source } => (
                    "SECRET_MISSING",
                    format!("代理凭据来源不存在（来源类型：{source:?}）。"),
                ),
                SecretResolveError::Empty { source } => (
                    "SECRET_EMPTY",
                    format!("代理凭据内容为空（来源类型：{source:?}）。"),
                ),
                SecretResolveError::TooLarge { limit } => (
                    "SECRET_TOO_LARGE",
                    format!("代理凭据超过 {limit} 字节上限。"),
                ),
                SecretResolveError::InvalidUtf8 => (
                    "SECRET_ENCODING_INVALID",
                    "代理凭据不是有效 UTF-8 文本。".to_owned(),
                ),
                SecretResolveError::Io => (
                    "SECRET_READ_FAILED",
                    "无法读取代理凭据文件，请检查文件和权限。".to_owned(),
                ),
                SecretResolveError::InvalidProxyUrl => (
                    "PROXY_URL_INVALID",
                    "代理 URL 格式无效，请重新填写完整地址。".to_owned(),
                ),
                SecretResolveError::UnsupportedProxyScheme => (
                    "PROXY_SCHEME_UNSUPPORTED",
                    "代理协议不受支持，请使用 socks5 或 socks5h。".to_owned(),
                ),
            };
            Diagnostic::new("secret", code, message).with_resource(outbound.as_str())
        }
        SecretValidationError::BootstrapForbidden { upstream, .. } => Diagnostic::new(
            "secret",
            "PROXY_BOOTSTRAP_CONFLICT",
            "socks5h 代理与此上游的 bootstrap 设置冲突，请调整解析方式。",
        )
        .with_resource(upstream.as_str()),
    }
}

/// 变更被拒绝的安全诊断；条目名来自当前配置或本次提交的 name 字段，属于可公开的资源标识。
pub(super) fn edit_diagnostic(error: &EditError) -> Diagnostic {
    let (code, message) = match error {
        EditError::Validation(report) => return config_diagnostic(report),
        EditError::Budget => (
            "CANDIDATE_BUDGET_EXCEEDED",
            "一次保存须包含 1–128 项修改且不超过 2 MiB。",
        ),
        EditError::DuplicateTarget => (
            "CONFIG_DUPLICATE_TARGET",
            "同一次保存重复修改了同一条目，请合并修改。",
        ),
        EditError::NotFound => (
            "CONFIG_ENTRY_NOT_FOUND",
            "待修改的条目已不存在，请刷新后重新编辑。",
        ),
        EditError::Layout { module, name } => {
            let diagnostic = Diagnostic::new(
                "configuration",
                "CONFIG_SOURCE_LAYOUT_UNSUPPORTED",
                format!(
                    "配置文件中 {module} 模块的该条目无法安全改写（例如含 YAML 锚点或别名），请手动编辑配置文件后重新加载。"
                ),
            );
            return match name {
                Some(name) => diagnostic.with_resource(name.as_str()),
                None => diagnostic,
            };
        }
        EditError::Internal => (
            "CONFIG_CANDIDATE_INTERNAL",
            "候选配置无法在类型化值与源配置之间转换，请凭请求 ID 检查服务日志。",
        ),
    };
    Diagnostic::new("configuration", code, message)
}

/// 校验正文可能包含用户提交内容，仅输出类别、已知字段路径和数量。
pub(super) fn config_diagnostic(report: &ConfigErrorReport) -> Diagnostic {
    let Some(error) = report.first() else {
        return Diagnostic::new(
            "configuration",
            "CONFIG_VALIDATION_FAILED",
            "配置校验失败，未提供可公开的字段详情。",
        );
    };
    let (code, reason) = match error.kind {
        ConfigErrorKind::Parse => ("CONFIG_PARSE_FAILED", "配置语法或数据类型无效"),
        ConfigErrorKind::UnsupportedVersion => ("CONFIG_VERSION_UNSUPPORTED", "配置版本不受支持"),
        ConfigErrorKind::MissingField => ("CONFIG_FIELD_MISSING", "缺少必填字段"),
        ConfigErrorKind::InvalidValue => ("CONFIG_VALUE_INVALID", "字段取值无效"),
        ConfigErrorKind::UnknownField => ("CONFIG_FIELD_UNKNOWN", "存在不支持的字段"),
        ConfigErrorKind::Duplicate => ("CONFIG_DUPLICATE", "名称或定义重复"),
        ConfigErrorKind::MissingReference => (
            "CONFIG_REFERENCE_MISSING",
            "引用的资源不存在，请核对资源名称",
        ),
        ConfigErrorKind::WrongReferenceKind => ("CONFIG_REFERENCE_KIND", "引用的资源类型不匹配"),
        ConfigErrorKind::Cycle => ("CONFIG_REFERENCE_CYCLE", "资源引用形成循环"),
        ConfigErrorKind::Constraint => ("CONFIG_CONSTRAINT", "字段组合不满足配置约束"),
        ConfigErrorKind::UnsupportedFeature => {
            ("CONFIG_FEATURE_UNSUPPORTED", "使用了当前不支持的功能")
        }
        ConfigErrorKind::BindConflict => ("CONFIG_BIND_CONFLICT", "监听地址或端口定义冲突"),
        ConfigErrorKind::Secret => (
            "CONFIG_SECRET_INVALID",
            "凭据引用无效或不可读取，请检查环境变量、凭据文件或重新填写凭据",
        ),
        ConfigErrorKind::Snapshot => ("CONFIG_SNAPSHOT_FAILED", "配置副本无法读取或写入"),
        ConfigErrorKind::Migration => ("CONFIG_MIGRATION_FAILED", "配置迁移失败"),
    };
    let path = safe_config_path(&error.path).unwrap_or_else(|| "配置".to_owned());
    Diagnostic::new(
        "configuration",
        code,
        format!(
            "{path}：{reason}（共 {} 项校验错误）。",
            report.errors.len()
        ),
    )
}

fn safe_config_path(path: &str) -> Option<String> {
    const FIELDS: &str = "version work path rules_path rule_set_max_rules rule_set_max_size_bytes database type records_path logs enable level webui address port public_origin users name password_hash listener addresses strategy upstream upstreams default_upstream rules hosts outbound rule_set clients client_id match ips dns cache statistics retention days grace_days reference_size_bytes format source url proxy proxy_url auto_update update_interval server bootstrap timeout retries tls mode certificate private_key routes endpoints client_ip ecs policy custom_ip force override use_client_subnet subnet ipv4 ipv6 query_types domain suffix keyword regex selectors selector rulesets memory max_size_bytes failure_ttl optimistic enabled answer_ttl max_age persistence snapshot_interval resolve_log custom allow_insecure secret env file group members domains ";
    if path.is_empty() || path.len() > 256 {
        return None;
    }
    for part in path.split(['.', '[', ']']).filter(|part| !part.is_empty()) {
        if !part.bytes().all(|byte| byte.is_ascii_digit())
            && !FIELDS.split_whitespace().any(|field| field == part)
        {
            return None;
        }
    }
    Some(path.to_owned())
}

pub(super) fn control_diagnostic(error: &ControlError) -> Diagnostic {
    let (code, message) = match error {
        ControlError::Busy => (
            "RUNTIME_QUEUE_FULL",
            "运行配置应用队列已满，请等待当前操作结束。",
        ),
        ControlError::Unavailable => (
            "RUNTIME_OWNER_UNAVAILABLE",
            "运行配置控制器已停止或不可用。",
        ),
        ControlError::Expired | ControlError::Apply(ServiceReloadError::Timeout) => (
            "RUNTIME_APPLY_TIMEOUT",
            "配置应用超过截止时间；资源准备和运行切换共用时间预算。",
        ),
        ControlError::RevisionConflict { .. }
        | ControlError::InvalidCandidateRevision
        | ControlError::Apply(ServiceReloadError::InvalidRevision { .. }) => (
            "RUNTIME_REVISION_CONFLICT",
            "运行配置版本发生冲突，请重新读取当前配置。",
        ),
        ControlError::OutcomeUnknown => (
            "RUNTIME_OUTCOME_UNKNOWN",
            "配置应用结果未知，请刷新配置状态确认后再操作。",
        ),
        ControlError::Apply(ServiceReloadError::Logging(_)) => (
            "LOGGING_APPLY_FAILED",
            "日志配置应用失败，请检查日志目录是否存在、可写及过滤配置是否有效。",
        ),
        ControlError::Apply(ServiceReloadError::LoggingCompensation(_)) => (
            "LOGGING_COMPENSATION_FAILED",
            "运行态切换失败且旧日志配置未能恢复，需人工核对。",
        ),
        ControlError::Apply(ServiceReloadError::Bind(_)) => (
            "LISTENER_BIND_FAILED",
            "监听地址或端口绑定失败，请检查端口占用、地址和权限。",
        ),
        ControlError::Apply(ServiceReloadError::Endpoint(_)) => (
            "LISTENER_PREPARE_FAILED",
            "监听器准备失败，请检查协议、TLS 配置及端点设置。",
        ),
        ControlError::Apply(ServiceReloadError::Activation(_)) => (
            "RUNTIME_ACTIVATION_FAILED",
            "候选运行态激活失败，旧运行态未被替换。",
        ),
        ControlError::Apply(ServiceReloadError::MissingDnsCore) => (
            "RUNTIME_DNS_CORE_MISSING",
            "当前运行态缺少 DNS 核心，无法切换配置。",
        ),
        ControlError::Apply(ServiceReloadError::Task(_)) => (
            "RUNTIME_TASK_FAILED",
            "运行任务注册失败，请检查服务是否正在关闭。",
        ),
        ControlError::Apply(ServiceReloadError::CacheSnapshot) => (
            "CACHE_SNAPSHOT_PREPARE_FAILED",
            "缓存快照模块未能准备配置切换。",
        ),
        ControlError::Apply(ServiceReloadError::RestartRequired { .. }) => (
            "RESTART_REQUIRED",
            "修改了进程启动级配置，需要重启服务，不能热应用。",
        ),
    };
    Diagnostic::new("runtime_apply", code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::validate::ConfigError;

    #[test]
    fn management_error_stays_below_large_result_threshold() {
        assert!(std::mem::size_of::<ManagementError>() < 128);
    }

    #[test]
    fn file_changed_carries_disk_status_only() {
        let error = ManagementError::from(StoreError::FileChanged(
            crate::config::disk::DiskStatus::Modified,
        ));
        assert!(matches!(error.code, ErrorCode::ConfigFileChanged));
        assert!(matches!(
            error.disk,
            Some(DiskState {
                status: super::super::contract::DiskStatus::Modified
            })
        ));
        assert!(ManagementError::from(StoreError::Busy).disk.is_none());
    }

    #[test]
    fn validation_does_not_echo_arbitrary_paths_or_messages() {
        let mut report = ConfigErrorReport::default();
        report.push(ConfigError::new(
            ConfigErrorKind::Secret,
            "secret-token",
            "password=hidden https://private/?token=hidden",
        ));
        let diagnostic = config_diagnostic(&report);
        let encoded = serde_json::to_string(&diagnostic).unwrap();
        assert!(!encoded.contains("hidden"));
        assert!(!encoded.contains("secret-token"));
        assert_eq!(diagnostic.code, "CONFIG_SECRET_INVALID");
        assert_eq!(
            safe_config_path("strategy[0].default_upstream").as_deref(),
            Some("strategy[0].default_upstream")
        );
    }

    #[test]
    fn persistence_does_not_echo_os_error_payload() {
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "secret-path-token");
        let diagnostic =
            crate::config::disk::io_diagnostic("CONFIG_FILE_WRITE_FAILED", "写入失败", &error);
        assert!(diagnostic.message.contains("PermissionDenied"));
        assert!(!diagnostic.message.contains("secret-path-token"));
    }
}
