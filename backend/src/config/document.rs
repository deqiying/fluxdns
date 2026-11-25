//! 内存配置文档：唯一的配置权威，也是当前 Runtime 的来源（启动级字段除外）。

use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::contract::ConfigV2;

/// 一份已严格解析的配置正文及其派生信息。
///
/// `digest` 是唯一的配置版本（正文 SHA-256，64 位小写十六进制）；`runtime_revision` 只用于
/// 读接口判断文档与 Runtime 是否同代，不作为写入前置条件。
pub(crate) struct ConfigDocument {
    pub(crate) text: Arc<str>,
    pub(crate) digest: String,
    pub(crate) config: Arc<ConfigV2>,
    pub(crate) runtime_revision: u64,
    /// 已写入配置但需重启进程才生效的启动级组件（`database`、`webui`、`dns.resolve_log`）。
    pub(crate) restart_required: Vec<&'static str>,
}

impl ConfigDocument {
    /// 用已经严格解析过的配置构造文档，避免同一正文重复解析。
    pub(crate) fn from_parsed(
        text: &str,
        config: ConfigV2,
        runtime_revision: u64,
        restart_required: Vec<&'static str>,
    ) -> Self {
        Self {
            text: Arc::from(text),
            digest: digest(text.as_bytes()),
            config: Arc::new(config),
            runtime_revision,
            restart_required,
        }
    }
}

/// 配置正文摘要；磁盘观测与内存文档使用同一算法比较。
pub(crate) fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
