//! `awaken-eval <dataset.json>` — replay a dataset through the real runtime, print
//! the scored report as JSON, and exit non-zero if any case failed. This is the
//! process entry point the e2e harness drives.

use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: awaken-eval <dataset.json>");
        return ExitCode::from(2);
    };
    let dataset = match awaken_eval::store::load_dataset(&PathBuf::from(&path)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to load dataset {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let report = awaken_eval::replay::run_dataset(&dataset).await;
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("failed to serialize report: {e}");
            return ExitCode::from(2);
        }
    }
    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        // A failed expectation is a non-zero exit so CI/e2e can gate on it.
        ExitCode::FAILURE
    }
}
