use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken-worker: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let mut server = None;
    let mut config = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--server" => server = args.next(),
            "--config" => config = args.next().map(PathBuf::from),
            "--help" | "-h" => {
                println!("awaken-worker --config PATH [--server URL]");
                return Ok(());
            }
            value if value.starts_with("--server=") => server = Some(value[9..].to_owned()),
            value if value.starts_with("--config=") => config = Some(PathBuf::from(&value[9..])),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let config = config.ok_or_else(|| "--config PATH is required".to_owned())?;
    let deployment = awaken_worker::WorkerDaemonConfig::load(&config, server)?;
    awaken_observability::init(&Default::default());
    let result = deployment
        .build()?
        .run_until_shutdown()
        .await
        .map_err(|error| error.to_string());
    awaken_observability::shutdown();
    result
}
