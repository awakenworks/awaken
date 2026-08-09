use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    awaken_cli::run_service_binary(awaken_cli::ServiceRole::Coordinator).await
}
