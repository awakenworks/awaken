//! `awaken-eval` CLI.
//!
//! Two subcommands:
//!
//!   awaken-eval replay --fixtures <DIR> --report <FILE>
//!   awaken-eval check  --baseline <FILE> --new <FILE>
//!
//! `replay` loads every `*.json` fixture from `<DIR>`, runs them through
//! the bundled [`MockReplayer`], scores each outcome, and writes one NDJSON
//! line per fixture to `<FILE>`. Exit code is non-zero when any fixture
//! fails its expectation.
//!
//! `check` parses two NDJSON reports and compares them with
//! [`diff_against_baseline`]. Exit code is non-zero when the diff is not
//! clean (regression or fixture missing from the new run).

use std::path::PathBuf;
use std::process::ExitCode;

use awaken_eval::{
    MockReplayer, ReplayReport, diff_against_baseline, fixture::load_directory, read_ndjson_path,
    replay_all, score, write_ndjson_path,
};

const HELP: &str = "\
awaken-eval — fixture-driven replay and scoring framework

Usage:
  awaken-eval replay --fixtures <DIR> --report <FILE>
  awaken-eval check  --baseline <FILE> --new <FILE>
  awaken-eval --help

Subcommands:
  replay   Load fixtures from <DIR>, replay each through MockReplayer, score,
           and write one NDJSON line per fixture to <FILE>.
  check    Compare two NDJSON reports; exit non-zero on regression or missing
           fixture.
";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("awaken-eval: {err}");
            ExitCode::from(2)
        }
    }
}

async fn run(args: Vec<String>) -> Result<ExitCode, String> {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    }

    match args[0].as_str() {
        "replay" => replay_command(&args[1..]).await,
        "check" => check_command(&args[1..]).await,
        other => Err(format!(
            "unknown subcommand {other:?} (try `awaken-eval --help`)"
        )),
    }
}

async fn replay_command(args: &[String]) -> Result<ExitCode, String> {
    let mut fixtures: Option<PathBuf> = None;
    let mut report: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--fixtures" => {
                fixtures = Some(PathBuf::from(
                    iter.next().ok_or("--fixtures requires a value")?,
                ));
            }
            "--report" => {
                report = Some(PathBuf::from(
                    iter.next().ok_or("--report requires a value")?,
                ));
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    let fixtures_dir = fixtures.ok_or("--fixtures <DIR> is required")?;
    let report_path = report.ok_or("--report <FILE> is required")?;

    let fixture_set =
        load_directory(&fixtures_dir).map_err(|err| format!("loading fixtures: {err}"))?;
    if fixture_set.is_empty() {
        eprintln!(
            "awaken-eval: no fixtures matched in {}",
            fixtures_dir.display()
        );
    }

    let outcomes = replay_all(&MockReplayer::new(), &fixture_set).await;

    let mut reports: Vec<ReplayReport> = Vec::with_capacity(outcomes.len());
    for (outcome, fixture) in outcomes.iter().zip(fixture_set.iter()) {
        let failures = score(outcome, &fixture.expect);
        reports.push(ReplayReport::from_outcome(outcome, failures));
    }

    write_ndjson_path(&report_path, &reports).map_err(|err| format!("writing report: {err}"))?;

    let total = reports.len();
    let passed = reports.iter().filter(|r| r.passed).count();
    let failed = total - passed;
    println!(
        "awaken-eval: {total} fixture(s) replayed — {passed} passed, {failed} failed → {}",
        report_path.display()
    );

    if failed > 0 {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

async fn check_command(args: &[String]) -> Result<ExitCode, String> {
    let mut baseline: Option<PathBuf> = None;
    let mut new_path: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--baseline" => {
                baseline = Some(PathBuf::from(
                    iter.next().ok_or("--baseline requires a value")?,
                ));
            }
            "--new" => {
                new_path = Some(PathBuf::from(iter.next().ok_or("--new requires a value")?));
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    let baseline_path = baseline.ok_or("--baseline <FILE> is required")?;
    let new_path = new_path.ok_or("--new <FILE> is required")?;

    let baseline =
        read_ndjson_path(&baseline_path).map_err(|err| format!("reading baseline: {err}"))?;
    let new = read_ndjson_path(&new_path).map_err(|err| format!("reading new: {err}"))?;

    let summary = diff_against_baseline(&baseline, &new);
    println!(
        "awaken-eval check: {regressions} regression(s), {missing} missing, {added} added",
        regressions = summary.regressions(),
        missing = summary.missing(),
        added = summary.added(),
    );
    for entry in &summary.entries {
        println!(
            "  {kind:24} {id}",
            kind = match entry {
                awaken_eval::DiffEntry::Unchanged { .. } => "unchanged",
                awaken_eval::DiffEntry::Regression { .. } => "regression",
                awaken_eval::DiffEntry::Fixed { .. } => "fixed",
                awaken_eval::DiffEntry::StillFailing { .. } => "still_failing",
                awaken_eval::DiffEntry::MissingFromNew { .. } => "missing_from_new",
                awaken_eval::DiffEntry::NewlyAdded { .. } => "newly_added",
            },
            id = entry.fixture_id()
        );
    }

    if summary.is_clean() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}
