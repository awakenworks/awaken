//! `awaken-eval` CLI — offline fixture replay, baseline diff, and trace
//! curation. Operations that hit a running server (dataset CRUD, run
//! orchestration, online eval, trace import) live behind `/v1/eval/*`
//! and are not duplicated here — drive them with `curl` or any generated
//! API client. This binary stays scoped to flows that genuinely need to
//! run without a server: CI fixture canaries, local fixture authoring,
//! on-host trace curation.

use std::path::PathBuf;
use std::process::ExitCode;

use awaken_eval::{
    Expectation, Fixture, ReplayReport, RuntimeReplayer, diff_against_baseline,
    fixture::load_directory, read_ndjson_path, replay_all, score, trace_to_provider_script,
    validate_unique_report_keys, write_ndjson_path,
};
use awaken_ext_observability::trace_store::{TraceStore, file::FileTraceStore};

const HELP: &str = "\
awaken-eval — offline fixture replay, baseline diff, trace curation
  replay   --fixtures <DIR>  --report <FILE>
  check    --baseline <FILE> --new <FILE>
  curate   --trace-root <DIR> --run-id <RUN> --out <FILE>
           [--user-input <TEXT>] [--allow-unused]
Server-side dataset/run operations: POST /v1/eval/* — use curl or an HTTP client.
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
        "curate" => curate_command(&args[1..]).await,
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

    let outcomes = replay_all(&RuntimeReplayer::new(), &fixture_set).await;

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
    // Refuse to diff a corrupt report — duplicate fixture_ids would
    // otherwise collapse silently in the BTreeMap pairing.
    validate_unique_report_keys(&baseline).map_err(|err| format!("baseline: {err}"))?;
    validate_unique_report_keys(&new).map_err(|err| format!("new: {err}"))?;

    let summary = diff_against_baseline(&baseline, &new);
    println!(
        "awaken-eval check: {regressions} regression(s), {drift} drift, {missing} missing, {added} added",
        regressions = summary.regressions(),
        drift = summary.drift(),
        missing = summary.missing(),
        added = summary.added(),
    );
    for entry in &summary.entries {
        let kind = match entry {
            awaken_eval::DiffEntry::Unchanged { .. } => "unchanged",
            awaken_eval::DiffEntry::Regression { .. } => "regression",
            awaken_eval::DiffEntry::Fixed { .. } => "fixed",
            awaken_eval::DiffEntry::StillFailing { .. } => "still_failing",
            awaken_eval::DiffEntry::Drift { .. } => "drift",
            awaken_eval::DiffEntry::MissingFromNew { .. } => "missing_from_new",
            awaken_eval::DiffEntry::NewlyAdded { .. } => "newly_added",
        };
        if let awaken_eval::DiffEntry::Drift { fields, .. } = entry {
            println!(
                "  {kind:24} {id}  fields={fields:?}",
                id = entry.fixture_id(),
            );
        } else {
            println!("  {kind:24} {id}", id = entry.fixture_id());
        }
    }

    if summary.is_clean() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

async fn curate_command(args: &[String]) -> Result<ExitCode, String> {
    let mut trace_root: Option<PathBuf> = None;
    let mut run_id: Option<String> = None;
    let mut user_input: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut allow_unused = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--trace-root" => {
                trace_root = Some(PathBuf::from(
                    iter.next().ok_or("--trace-root requires a value")?,
                ));
            }
            "--run-id" => {
                run_id = Some(iter.next().ok_or("--run-id requires a value")?.into());
            }
            "--user-input" => {
                user_input = Some(iter.next().ok_or("--user-input requires a value")?.into());
            }
            "--out" => {
                out = Some(PathBuf::from(iter.next().ok_or("--out requires a value")?));
            }
            "--allow-unused" => {
                allow_unused = true;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    let trace_root = trace_root.ok_or("--trace-root <DIR> is required")?;
    let run_id = run_id.ok_or("--run-id <RUN> is required")?;
    let out_path = out.ok_or("--out <FILE> is required")?;

    let store = FileTraceStore::new(&trace_root)
        .map_err(|err| format!("opening trace store at {}: {err}", trace_root.display()))?;
    let events = store
        .read(&run_id)
        .map_err(|err| format!("reading trace {run_id}: {err}"))?;
    let conversion = trace_to_provider_script(&events).map_err(|err| format!("curating: {err}"))?;

    // Explicit `--user-input` wins (operator may want to rephrase the
    // prompt for the fixture); otherwise fall back to the user message
    // recovered from `request_messages` capture.
    let user_input = match user_input.or(conversion.user_input.clone()) {
        Some(text) => text,
        None => {
            return Err(
                "--user-input <TEXT> is required (originating trace did not capture \
                 request_messages — enable ContentCapture::Enabled on the run)"
                    .to_string(),
            );
        }
    };

    // Operators add expectations by editing the JSON after curate emits
    // the skeleton — there's nothing to score the fixture against until
    // they do. `description` is left blank for the same reason.
    let fixture = Fixture {
        id: run_id.clone(),
        description: Some(format!("Curated from trace {run_id}")),
        user_input,
        provider_script: conversion.provider_script,
        source_run_id: Some(run_id),
        source_model_id: conversion.source_model_id,
        allow_unused_provider_script: allow_unused,
        mock_response: Default::default(),
        expect: Expectation::default(),
        continued_turns: vec![],
    };

    let json = serde_json::to_string_pretty(&fixture)
        .map_err(|err| format!("serialising fixture: {err}"))?;
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("creating {}: {err}", parent.display()))?;
    }
    std::fs::write(&out_path, json)
        .map_err(|err| format!("writing fixture to {}: {err}", out_path.display()))?;

    let inferences = fixture.provider_script.len();
    println!(
        "awaken-eval: curated {inferences} inference(s) from trace {} → {}",
        fixture.source_run_id.as_deref().unwrap_or("?"),
        out_path.display()
    );
    Ok(ExitCode::SUCCESS)
}
