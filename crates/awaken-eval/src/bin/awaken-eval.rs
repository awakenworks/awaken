//! `awaken-eval` CLI — see HELP for the subcommand surface.

use std::path::PathBuf;
use std::process::ExitCode;

use awaken_eval::{
    DatasetSpec, Expectation, Fixture, MockReplayer, ReplayReport, diff_against_baseline,
    fixture::load_directory, read_ndjson_path, replay_all, score, trace_to_provider_script,
    write_ndjson_path,
};
use awaken_ext_observability::trace_store::{TraceStore, file::FileTraceStore};
use serde_json::{Value, json};

const HELP: &str = "\
awaken-eval — fixture-driven replay and scoring framework

Offline:
  replay --fixtures <DIR> --report <FILE>
  check  --baseline <FILE> --new <FILE>
  curate --trace-root <DIR> --run-id <RUN> [--user-input <TEXT>] --out <FILE>

Server (--server <URL> or $AWAKEN_SERVER_URL, --bearer or $AWAKEN_BEARER_TOKEN):
  push   --dataset <ID> --fixtures <DIR> [--force]   wholesale overwrite (--force required if exists)
  append --dataset <ID> --fixture-file <FILE>        atomic single-fixture append (CAS on revision)
  run    --dataset <ID> [--baseline <RUN>] [--out <FILE>]
  pull   --run <RUN> [--baseline <RUN>] --out <FILE>
  online --prompt <TEXT> --models <ID,ID,...> [--persist] [--out <FILE>]
         ad-hoc online evaluation: prompt × N models in parallel
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
        "push" => push_command(&args[1..]).await,
        "run" => run_remote_command(&args[1..]).await,
        "pull" => pull_command(&args[1..]).await,
        "online" => online_command(&args[1..]).await,
        "append" => append_command(&args[1..]).await,
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

// ── Server-talking subcommands (ADR-0032 D8) ──────────────────────────────

/// Resolve the server base URL from `--server <URL>` or
/// `AWAKEN_SERVER_URL`. Errors when neither is set so the CLI fails fast
/// instead of building a request to the empty string.
fn resolve_server(explicit: Option<String>) -> Result<String, String> {
    let url = explicit
        .or_else(|| std::env::var("AWAKEN_SERVER_URL").ok())
        .ok_or_else(|| "--server <URL> or AWAKEN_SERVER_URL is required".to_string())?;
    Ok(url.trim_end_matches('/').to_string())
}

fn resolve_bearer(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| std::env::var("AWAKEN_BEARER_TOKEN").ok())
}

/// Build a [`reqwest::Client`] preloaded with the admin bearer header
/// when one is available. Keeping it as a helper means each subcommand
/// can attach the same auth model without repeating itself.
fn http_client(bearer: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder();
    if let Some(token) = bearer {
        let mut headers = reqwest::header::HeaderMap::new();
        let value = format!("Bearer {token}");
        let header = reqwest::header::HeaderValue::from_str(&value)
            .map_err(|err| format!("invalid bearer token: {err}"))?;
        headers.insert(reqwest::header::AUTHORIZATION, header);
        builder = builder.default_headers(headers);
    }
    builder
        .build()
        .map_err(|err| format!("building HTTP client: {err}"))
}

async fn ok_or_status(resp: reqwest::Response) -> Result<reqwest::Response, String> {
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("server returned {status}: {body}"));
    }
    Ok(resp)
}

async fn push_command(args: &[String]) -> Result<ExitCode, String> {
    let mut server: Option<String> = None;
    let mut bearer: Option<String> = None;
    let mut dataset_id: Option<String> = None;
    let mut fixtures_dir: Option<PathBuf> = None;
    let mut description: Option<String> = None;
    let mut force = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().ok_or("--server requires a value")?.into()),
            "--bearer" => bearer = Some(iter.next().ok_or("--bearer requires a value")?.into()),
            "--dataset" => {
                dataset_id = Some(iter.next().ok_or("--dataset requires a value")?.into());
            }
            "--fixtures" => {
                fixtures_dir = Some(PathBuf::from(
                    iter.next().ok_or("--fixtures requires a value")?,
                ));
            }
            "--description" => {
                description = Some(iter.next().ok_or("--description requires a value")?.into());
            }
            "--force" => force = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let server = resolve_server(server)?;
    let bearer = resolve_bearer(bearer);
    let dataset_id = dataset_id.ok_or("--dataset <ID> is required")?;
    let fixtures_dir = fixtures_dir.ok_or("--fixtures <DIR> is required")?;

    let fixtures = load_directory(&fixtures_dir)
        .map_err(|err| format!("loading fixtures from {}: {err}", fixtures_dir.display()))?;
    if fixtures.is_empty() {
        return Err(format!("no fixtures found in {}", fixtures_dir.display()));
    }
    let spec = DatasetSpec {
        description: description.unwrap_or_default(),
        fixtures,
    };

    let client = http_client(bearer.as_deref())?;
    // POST first to cover the common "first push" case in one round-trip.
    // If the dataset already exists, fall through to a GET+PUT cycle
    // with bounded retries — a concurrent admin edit between our GET and
    // PUT would otherwise surface a fresh 409 to the operator.
    let post_url = format!("{server}/v1/eval/datasets");
    let body = json!({ "id": dataset_id, "spec": spec });
    let resp = client
        .post(&post_url)
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("POST {post_url}: {err}"))?;
    let status = resp.status();
    if status != reqwest::StatusCode::CONFLICT {
        ok_or_status(resp).await?;
        println!("awaken-eval: created dataset {dataset_id}");
        return Ok(ExitCode::SUCCESS);
    }

    // `push` is whole-spec overwrite; require --force when the dataset
    // already exists so concurrent fixtures aren't silently dropped.
    if !force {
        let get_url = format!("{server}/v1/eval/datasets/{dataset_id}");
        let existing = ok_or_status(
            client
                .get(&get_url)
                .send()
                .await
                .map_err(|err| format!("GET {get_url}: {err}"))?,
        )
        .await?;
        let existing_json: Value = existing
            .json()
            .await
            .map_err(|err| format!("decoding existing dataset: {err}"))?;
        let n = existing_json
            .get("spec")
            .and_then(|s| s.get("fixtures"))
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        return Err(format!(
            "dataset {dataset_id} already exists with {n} fixture(s). \
             Pass --force to overwrite, or use `awaken-eval append` to add a single fixture."
        ));
    }

    // Update path with bounded retry for revision races.
    const MAX_REVISION_RETRIES: usize = 3;
    let get_url = format!("{server}/v1/eval/datasets/{dataset_id}");
    let put_url = format!("{server}/v1/eval/datasets/{dataset_id}");
    let mut last_revision: u64 = 0;
    for attempt in 0..MAX_REVISION_RETRIES {
        let existing = ok_or_status(
            client
                .get(&get_url)
                .send()
                .await
                .map_err(|err| format!("GET {get_url}: {err}"))?,
        )
        .await?;
        let existing_json: Value = existing
            .json()
            .await
            .map_err(|err| format!("decoding existing dataset: {err}"))?;
        let revision = existing_json
            .get("meta")
            .and_then(|m| m.get("revision"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        last_revision = revision;
        let put_body = json!({ "expected_revision": revision, "spec": spec });
        let put_resp = client
            .put(&put_url)
            .json(&put_body)
            .send()
            .await
            .map_err(|err| format!("PUT {put_url}: {err}"))?;
        if put_resp.status() == reqwest::StatusCode::CONFLICT {
            // Concurrent admin edit — re-read and try again with the
            // fresh revision. Bounded retries so a perpetually-edited
            // dataset doesn't trap the CLI in an infinite loop.
            tracing::warn!(
                attempt = attempt + 1,
                revision,
                "dataset revision conflict, retrying"
            );
            continue;
        }
        ok_or_status(put_resp).await?;
        println!("awaken-eval: updated dataset {dataset_id} at revision {revision}");
        return Ok(ExitCode::SUCCESS);
    }
    Err(format!(
        "dataset {dataset_id} kept changing during update (last seen revision {last_revision}); \
         tried {MAX_REVISION_RETRIES} times"
    ))
}

async fn run_remote_command(args: &[String]) -> Result<ExitCode, String> {
    let mut server: Option<String> = None;
    let mut bearer: Option<String> = None;
    let mut dataset_id: Option<String> = None;
    let mut baseline_run_id: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().ok_or("--server requires a value")?.into()),
            "--bearer" => bearer = Some(iter.next().ok_or("--bearer requires a value")?.into()),
            "--dataset" => {
                dataset_id = Some(iter.next().ok_or("--dataset requires a value")?.into());
            }
            "--baseline" => {
                baseline_run_id = Some(iter.next().ok_or("--baseline requires a value")?.into());
            }
            "--out" => out = Some(PathBuf::from(iter.next().ok_or("--out requires a value")?)),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let server = resolve_server(server)?;
    let bearer = resolve_bearer(bearer);
    let dataset_id = dataset_id.ok_or("--dataset <ID> is required")?;

    let client = http_client(bearer.as_deref())?;
    let url = format!("{server}/v1/eval/runs");
    let body = if let Some(baseline) = &baseline_run_id {
        json!({ "dataset_id": dataset_id, "baseline_run_id": baseline })
    } else {
        json!({ "dataset_id": dataset_id })
    };
    let resp = ok_or_status(
        client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("POST {url}: {err}"))?,
    )
    .await?;
    let value: Value = resp
        .json()
        .await
        .map_err(|err| format!("decoding eval run response: {err}"))?;
    let run_id = value
        .get("run")
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let item_count = value
        .get("run")
        .and_then(|r| r.get("items"))
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let passed = value
        .get("run")
        .and_then(|r| r.get("items"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|i| {
                    i.get("report")
                        .and_then(|r| r.get("passed"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    println!("awaken-eval: run {run_id} — {passed}/{item_count} fixture(s) passed");
    if let Some(out_path) = out {
        write_value(&out_path, &value)?;
        println!("awaken-eval: wrote {} → {}", out_path.display(), run_id);
    }
    Ok(if passed < item_count {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

async fn pull_command(args: &[String]) -> Result<ExitCode, String> {
    let mut server: Option<String> = None;
    let mut bearer: Option<String> = None;
    let mut run_id: Option<String> = None;
    let mut baseline: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().ok_or("--server requires a value")?.into()),
            "--bearer" => bearer = Some(iter.next().ok_or("--bearer requires a value")?.into()),
            "--run" => run_id = Some(iter.next().ok_or("--run requires a value")?.into()),
            "--baseline" => {
                baseline = Some(iter.next().ok_or("--baseline requires a value")?.into());
            }
            "--out" => out = Some(PathBuf::from(iter.next().ok_or("--out requires a value")?)),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let server = resolve_server(server)?;
    let bearer = resolve_bearer(bearer);
    let run_id = run_id.ok_or("--run <RUN_ID> is required")?;
    let out_path = out.ok_or("--out <FILE> is required")?;

    let client = http_client(bearer.as_deref())?;
    let url = if let Some(b) = baseline {
        format!("{server}/v1/eval/runs/{run_id}?baseline={b}")
    } else {
        format!("{server}/v1/eval/runs/{run_id}")
    };
    let resp = ok_or_status(
        client
            .get(&url)
            .send()
            .await
            .map_err(|err| format!("GET {url}: {err}"))?,
    )
    .await?;
    let value: Value = resp
        .json()
        .await
        .map_err(|err| format!("decoding pull response: {err}"))?;
    write_value(&out_path, &value)?;
    println!("awaken-eval: pulled {run_id} → {}", out_path.display());
    Ok(ExitCode::SUCCESS)
}

fn write_value(path: &std::path::Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("creating {}: {err}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(value)
        .map_err(|err| format!("serialising response: {err}"))?;
    std::fs::write(path, json).map_err(|err| format!("writing {}: {err}", path.display()))?;
    Ok(())
}

async fn online_command(args: &[String]) -> Result<ExitCode, String> {
    let mut server: Option<String> = None;
    let mut bearer: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut models: Option<Vec<String>> = None;
    let mut persist: Option<bool> = None;
    let mut out: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().ok_or("--server requires a value")?.into()),
            "--bearer" => bearer = Some(iter.next().ok_or("--bearer requires a value")?.into()),
            "--prompt" => prompt = Some(iter.next().ok_or("--prompt requires a value")?.into()),
            "--models" => {
                let csv: &str = iter.next().ok_or("--models requires a value")?;
                models = Some(csv.split(',').map(|s| s.trim().to_string()).collect());
            }
            "--persist" => {
                let v: &str = iter.next().ok_or("--persist requires true|false")?;
                persist = Some(matches!(v, "true" | "1" | "yes"));
            }
            "--out" => out = Some(PathBuf::from(iter.next().ok_or("--out requires a value")?)),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let server = resolve_server(server)?;
    let bearer = resolve_bearer(bearer);
    let prompt = prompt.ok_or("--prompt <TEXT> is required")?;
    let models = models.ok_or("--models <ID,ID,...> is required")?;
    if models.is_empty() || models.iter().any(|m| m.is_empty()) {
        return Err("--models must contain at least one non-empty model id".into());
    }

    let client = http_client(bearer.as_deref())?;
    let url = format!("{server}/v1/eval/online");
    let mut body = json!({
        "user_input": prompt,
        "models": models,
    });
    if let Some(p) = persist {
        body["persist"] = json!(p);
    }
    let resp = ok_or_status(
        client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("POST {url}: {err}"))?,
    )
    .await?;
    let value: Value = resp
        .json()
        .await
        .map_err(|err| format!("decoding online response: {err}"))?;

    let run_id = value
        .get("run")
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let items = value
        .get("run")
        .and_then(|r| r.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let persisted = value
        .get("persisted")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let persisted_marker = if persisted {
        " (persisted)"
    } else {
        " (ephemeral)"
    };
    println!("awaken-eval online: run {run_id}{persisted_marker}");
    for item in &items {
        let model = item
            .get("cell")
            .and_then(|c| c.get("model_id"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let passed = item
            .get("report")
            .and_then(|r| r.get("passed"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let tokens = item
            .get("report")
            .and_then(|r| r.get("total_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let trace = item
            .get("trace_run_id")
            .and_then(Value::as_str)
            .unwrap_or("-");
        println!(
            "  {model:30}  {} tokens={tokens:>6}  trace={trace}",
            if passed { "PASS" } else { "FAIL" }
        );
    }

    if let Some(out_path) = out {
        write_value(&out_path, &value)?;
        println!("awaken-eval: wrote {} → {}", out_path.display(), run_id);
    }
    let any_failed = items.iter().any(|i| {
        !i.get("report")
            .and_then(|r| r.get("passed"))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    });
    Ok(if any_failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

async fn append_command(args: &[String]) -> Result<ExitCode, String> {
    let mut server: Option<String> = None;
    let mut bearer: Option<String> = None;
    let mut dataset_id: Option<String> = None;
    let mut fixture_file: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().ok_or("--server requires a value")?.into()),
            "--bearer" => bearer = Some(iter.next().ok_or("--bearer requires a value")?.into()),
            "--dataset" => {
                dataset_id = Some(iter.next().ok_or("--dataset requires a value")?.into());
            }
            "--fixture-file" => {
                fixture_file = Some(PathBuf::from(
                    iter.next().ok_or("--fixture-file requires a value")?,
                ));
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let server = resolve_server(server)?;
    let bearer = resolve_bearer(bearer);
    let dataset_id = dataset_id.ok_or("--dataset <ID> is required")?;
    let fixture_file = fixture_file.ok_or("--fixture-file <FILE> is required")?;

    let fixture_json = std::fs::read_to_string(&fixture_file)
        .map_err(|err| format!("reading {}: {err}", fixture_file.display()))?;
    let fixture: Value = serde_json::from_str(&fixture_json)
        .map_err(|err| format!("parsing {}: {err}", fixture_file.display()))?;

    let client = http_client(bearer.as_deref())?;
    let get_url = format!("{server}/v1/eval/datasets/{dataset_id}");
    let existing = ok_or_status(
        client
            .get(&get_url)
            .send()
            .await
            .map_err(|err| format!("GET {get_url}: {err}"))?,
    )
    .await?;
    let existing_json: Value = existing
        .json()
        .await
        .map_err(|err| format!("decoding existing dataset: {err}"))?;
    let revision = existing_json
        .get("meta")
        .and_then(|m| m.get("revision"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let url = format!("{server}/v1/eval/datasets/{dataset_id}/fixtures");
    let body = json!({ "fixture": fixture, "expected_revision": revision });
    let resp = ok_or_status(
        client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("POST {url}: {err}"))?,
    )
    .await?;
    let result: Value = resp
        .json()
        .await
        .map_err(|err| format!("decoding append response: {err}"))?;
    let new_rev = result
        .get("meta")
        .and_then(|m| m.get("revision"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let n = result
        .get("spec")
        .and_then(|s| s.get("fixtures"))
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    println!("awaken-eval: appended to {dataset_id} → revision {new_rev}, {n} fixture(s) total");
    Ok(ExitCode::SUCCESS)
}
