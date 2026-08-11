//! `awaken` — product launcher, operator commands, and AllInOne startup.

use std::process::ExitCode;

use awaken_cli::ServiceRole;
use awaken_cli::config::{ConfigOverrides, ResolvedDeployment};
use awaken_cli::console::{self, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let command = match console::parse_args(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("awaken: {error}\n");
            console::print_help();
            return ExitCode::FAILURE;
        }
    };

    match run(command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Help => {
            console::print_help();
            Ok(())
        }
        Command::Version => {
            println!("awaken {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Config { json, config_path } => {
            let deployment = ResolvedDeployment::load(ConfigOverrides {
                config_path,
                ..Default::default()
            })?;
            println!("{}", deployment.report(json).trim_end());
            Ok(())
        }
        Command::DoctorAcp { json } => {
            println!(
                "{}",
                awaken_cli::local_acp_diagnostics(json).await.trim_end()
            );
            Ok(())
        }
        Command::DatabaseMigrate { config_path } => awaken_cli::migrate_service(config_path).await,
        Command::ControlIamProfile => print_json(
            &awaken_control::management_authorization_profile(),
            "Control IAM profile",
        ),
        Command::ControlIamResourceProfile => print_json(
            &awaken_control::management_resource_authorization_profile(),
            "Control resource IAM profile",
        ),
        Command::ControlIamRuntimeProfile => print_json(
            &awaken_control::hosted_runtime_authorization_profile(),
            "Hosted Runtime IAM profile",
        ),
        Command::AllInOne(args) => awaken_cli::run_service(args, ServiceRole::AllInOne).await,
        Command::Control(args) => awaken_cli::run_service(args, ServiceRole::Control).await,
        Command::Coordinator(args) => awaken_cli::run_service(args, ServiceRole::Coordinator).await,
    }
}

fn print_json(value: &impl serde::Serialize, label: &str) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| format!("serialize {label}: {error}"))?
    );
    Ok(())
}
