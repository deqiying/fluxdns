use std::process::ExitCode;

pub mod app;
pub mod cache;
pub mod config;
pub mod dns;
pub mod observability;
pub mod policy;
pub mod ports;
pub mod resource;
pub mod runtime;
pub mod service;
pub mod storage;
pub mod transport;
pub mod upstream;

/// 确保依赖默认 builder 的 rustls 客户端已安装进程级 crypto provider。
///
/// 并行或重复调用会保留最先安装的 provider，避免测试和运行时初始化顺序影响 TLS。
pub(crate) fn ensure_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    if observability::init_bootstrap().is_err() {
        return report_error(&app::AppError::new(
            app::AppErrorKind::RuntimeFatal,
            "bootstrap 日志初始化失败",
        ));
    }

    match app::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report_error(&error),
    }
}

fn report_error(error: &app::AppError) -> ExitCode {
    eprintln!(
        "FluxDNS 退出：kind={} message={}",
        error.kind(),
        error.safe_message()
    );
    ExitCode::from(error.exit_code().value())
}
