use std::process::ExitCode;

fn main() -> ExitCode {
    awaken_cli::block_on_service(awaken_cli::run_service_binary(
        awaken_cli::ServiceRole::Control,
    ))
}
