use std::process::ExitCode;

fn main() -> ExitCode {
    awaken_process_startup::block_on_service(|| {
        awaken_process_startup::run_service_binary(awaken_process_startup::ServiceRole::Control)
    })
}
