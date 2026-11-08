//! 跨配置、资源和管理接口传递的有界安全诊断；禁止从任意底层错误字符串直接构造。

use serde::{Deserialize, Serialize};

/// 只保存固定阶段/原因码、经过审查的说明及资源标识，不携带 URL、路径或秘密值。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub stage: String,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl Diagnostic {
    /// message 只能由固定文案及已审查的数字/枚举构成，不接受 Error::to_string()。
    pub(crate) fn new(stage: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage: stage.to_owned(),
            code: code.to_owned(),
            message: bounded_text(&message.into(), 1024),
            resource: None,
        }
    }

    /// 资源名是管理面已有的配置标识；控制字符和长度受限，不能传入来源地址。
    pub(crate) fn with_resource(mut self, resource: &str) -> Self {
        self.resource = Some(bounded_text(resource, 128));
        self
    }
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let mut result = String::new();
    for character in value.chars().filter(|character| !character.is_control()) {
        if result.len() + character.len_utf8() > max_bytes {
            break;
        }
        result.push(character);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_are_bounded_without_splitting_utf8() {
        let diagnostic = Diagnostic::new("resource_parse", "TEST", "测".repeat(1000))
            .with_resource(&format!("name\n{}", "源".repeat(100)));
        assert!(diagnostic.message.len() <= 1024);
        let resource = diagnostic.resource.unwrap();
        assert!(resource.len() <= 128);
        assert!(!resource.contains('\n'));
    }
}
