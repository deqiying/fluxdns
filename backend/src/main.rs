use std::process::ExitCode;

pub mod app;
pub mod config;
pub mod dns;
pub mod observability;
pub mod ports;
pub mod runtime;

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
