//! `awaken-eval <dataset.json>` — replay a dataset through the real runtime, print
//! the scored report as JSON, and exit non-zero if any case failed. This is the
//! process entry point the e2e harness drives.

use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("outcome-score") {
        return score_outcome(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("outcome-import-claude") {
        return import_claude(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("outcome-import-codex") {
        return import_codex(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("outcome-run-acp") {
        return run_outcome_acp(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("outcome-select-acp-failures") {
        return select_acp_failures(&args[1..]);
    }
    let Some(path) = args.first() else {
        eprintln!(
            "usage: awaken-eval <dataset.json> | \
             awaken-eval outcome-score <judge-dataset.json> <observations.json>"
        );
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

fn import_codex(args: &[String]) -> ExitCode {
    let [sessions_root, goals_db, output_path, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval outcome-import-codex <sessions-root> <goals.sqlite> <dataset.json> [limit]"
        );
        return ExitCode::from(2);
    };
    let limit = match rest {
        [] => 0,
        [limit] => match limit.parse::<usize>() {
            Ok(limit) => limit,
            Err(error) => {
                eprintln!("invalid import limit {limit:?}: {error}");
                return ExitCode::from(2);
            }
        },
        _ => {
            eprintln!(
                "usage: awaken-eval outcome-import-codex <sessions-root> <goals.sqlite> <dataset.json> [limit]"
            );
            return ExitCode::from(2);
        }
    };
    let (dataset, stats) = match awaken_eval::transcript_corpus::import_codex_goals(
        &PathBuf::from(sessions_root),
        &PathBuf::from(goals_db),
        "codex-goal-complete-private-v1",
        limit,
    ) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("failed to import Codex transcripts: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) =
        awaken_eval::store::save_judge_dataset(&PathBuf::from(output_path), &dataset)
    {
        eprintln!("failed to save Judge dataset {output_path}: {error}");
        return ExitCode::from(2);
    }
    match serde_json::to_string_pretty(&stats) {
        Ok(json) => eprintln!("{json}"),
        Err(error) => {
            eprintln!("failed to serialize import stats: {error}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

fn select_acp_failures(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path, output_path] = args else {
        eprintln!(
            "usage: awaken-eval outcome-select-acp-failures <dataset.json> <artifact.json> <output.json>"
        );
        return ExitCode::from(2);
    };
    let mut dataset = match awaken_eval::store::load_judge_dataset(&PathBuf::from(dataset_path)) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to load Judge dataset {dataset_path}: {error}");
            return ExitCode::from(2);
        }
    };
    let data = match std::fs::read_to_string(artifact_path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("failed to read ACP artifact {artifact_path}: {error}");
            return ExitCode::from(2);
        }
    };
    let artifact: awaken_eval::outcome_acp::AcpEvaluationArtifact =
        match serde_json::from_str(&data) {
            Ok(artifact) => artifact,
            Err(error) => {
                eprintln!("failed to parse ACP artifact {artifact_path}: {error}");
                return ExitCode::from(2);
            }
        };
    let failed_ids = artifact
        .batches
        .iter()
        .filter(|batch| batch.error.is_some())
        .flat_map(|batch| batch.case_ids.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    dataset.cases.retain(|case| failed_ids.contains(&case.id));
    dataset.name = format!("{}-acp-first-pass-failures", dataset.name);
    if dataset.cases.is_empty() {
        eprintln!("ACP artifact contains no failed batches");
        return ExitCode::from(1);
    }
    if let Err(error) =
        awaken_eval::store::save_judge_dataset(&PathBuf::from(output_path), &dataset)
    {
        eprintln!("failed to save failed-batch dataset {output_path}: {error}");
        return ExitCode::from(2);
    }
    eprintln!(
        "selected {} cases from failed ACP batches",
        dataset.cases.len()
    );
    ExitCode::SUCCESS
}

async fn run_outcome_acp(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path, argv_json, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval outcome-run-acp <dataset.json> <artifact.json> <argv-json> [batch-size]"
        );
        return ExitCode::from(2);
    };
    let batch_size = match rest {
        [] => 20,
        [value] => match value.parse::<usize>() {
            Ok(value) if value > 0 => value,
            Ok(_) => {
                eprintln!("batch-size must be positive");
                return ExitCode::from(2);
            }
            Err(error) => {
                eprintln!("invalid batch-size {value:?}: {error}");
                return ExitCode::from(2);
            }
        },
        _ => {
            eprintln!(
                "usage: awaken-eval outcome-run-acp <dataset.json> <artifact.json> <argv-json> [batch-size]"
            );
            return ExitCode::from(2);
        }
    };
    let argv: Vec<String> = match serde_json::from_str(argv_json) {
        Ok(argv) => argv,
        Err(error) => {
            eprintln!("invalid ACP argv JSON: {error}");
            return ExitCode::from(2);
        }
    };
    if argv.is_empty() {
        eprintln!("ACP argv must not be empty");
        return ExitCode::from(2);
    }
    let env: Vec<(String, String)> = match std::env::var("AWAKEN_EVAL_ACP_ENV_JSON") {
        Ok(value) => {
            match serde_json::from_str::<std::collections::BTreeMap<String, String>>(&value) {
                Ok(env) => env.into_iter().collect(),
                Err(error) => {
                    eprintln!("invalid AWAKEN_EVAL_ACP_ENV_JSON: {error}");
                    return ExitCode::from(2);
                }
            }
        }
        Err(std::env::VarError::NotPresent) => Vec::new(),
        Err(error) => {
            eprintln!("failed to read AWAKEN_EVAL_ACP_ENV_JSON: {error}");
            return ExitCode::from(2);
        }
    };
    let dataset = match awaken_eval::store::load_judge_dataset(&PathBuf::from(dataset_path)) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to load Judge dataset {dataset_path}: {error}");
            return ExitCode::from(2);
        }
    };
    let artifact = awaken_eval::outcome_acp::run_dataset(&dataset, argv, env, batch_size).await;
    let data = match serde_json::to_string_pretty(&artifact) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("failed to serialize ACP artifact: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = std::fs::write(artifact_path, data) {
        eprintln!("failed to save ACP artifact {artifact_path}: {error}");
        return ExitCode::from(2);
    }
    let report = awaken_eval::outcome_judge::score(&dataset, &artifact.observations);
    match serde_json::to_string_pretty(&report.metrics) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("failed to serialize Judge metrics: {error}");
            return ExitCode::from(2);
        }
    }
    if artifact.batches.iter().all(|batch| batch.error.is_none()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn import_claude(args: &[String]) -> ExitCode {
    let [transcript_root, output_path, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval outcome-import-claude <transcript-root> <dataset.json> [limit]"
        );
        return ExitCode::from(2);
    };
    let limit = match rest {
        [] => 120,
        [limit] => match limit.parse::<usize>() {
            Ok(limit) => limit,
            Err(error) => {
                eprintln!("invalid import limit {limit:?}: {error}");
                return ExitCode::from(2);
            }
        },
        _ => {
            eprintln!(
                "usage: awaken-eval outcome-import-claude <transcript-root> <dataset.json> [limit]"
            );
            return ExitCode::from(2);
        }
    };
    let (dataset, stats) = match awaken_eval::transcript_corpus::import_claude_goals(
        &PathBuf::from(transcript_root),
        "claude-goal-status-private-v1",
        limit,
    ) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("failed to import Claude transcripts: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) =
        awaken_eval::store::save_judge_dataset(&PathBuf::from(output_path), &dataset)
    {
        eprintln!("failed to save Judge dataset {output_path}: {error}");
        return ExitCode::from(2);
    }
    match serde_json::to_string_pretty(&stats) {
        Ok(json) => eprintln!("{json}"),
        Err(error) => {
            eprintln!("failed to serialize import stats: {error}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

fn score_outcome(args: &[String]) -> ExitCode {
    let [dataset_path, observations_path] = args else {
        eprintln!("usage: awaken-eval outcome-score <judge-dataset.json> <observations.json>");
        return ExitCode::from(2);
    };
    let dataset = match awaken_eval::store::load_judge_dataset(&PathBuf::from(dataset_path)) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to load Judge dataset {dataset_path}: {error}");
            return ExitCode::from(2);
        }
    };
    let observations =
        match awaken_eval::store::load_judge_observations(&PathBuf::from(observations_path)) {
            Ok(observations) => observations,
            Err(error) => {
                eprintln!("failed to load Judge observations {observations_path}: {error}");
                return ExitCode::from(2);
            }
        };
    let report = awaken_eval::outcome_judge::score(&dataset, &observations);
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("failed to serialize Judge report: {error}");
            return ExitCode::from(2);
        }
    }
    if report.metrics.decision_correct == report.metrics.total
        && report.metrics.schema_valid == report.metrics.total
        && report.metrics.unsafe_accepts == 0
        && report.unexpected_observation_ids.is_empty()
        && report.duplicate_observation_ids.is_empty()
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
