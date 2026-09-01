//! `awaken-eval <dataset.json>` — replay a dataset through the real runtime, print
//! the scored report as JSON, and exit non-zero if any case failed. This is the
//! process entry point the e2e harness drives.

use std::path::PathBuf;
use std::process::ExitCode;

mod admin_authoring_cli;

type AcpLaunchArgs = (Vec<String>, Vec<(String, String)>);

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("admin-authoring-run-live") {
        return admin_authoring_cli::run_live(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("admin-authoring-score") {
        return admin_authoring_cli::score(&args[1..]);
    }
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
    if args.first().map(String::as_str) == Some("compact-score") {
        return score_compact(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("compact-run-acp") {
        return run_compact_acp(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("memory-score") {
        return score_memory(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("memory-run-acp") {
        return run_memory_acp(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("benchmark-import-rewardbench2") {
        return import_rewardbench2(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("benchmark-run-pairwise-acp") {
        return run_pairwise_acp(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("benchmark-score-pairwise") {
        return score_pairwise(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("benchmark-compare-pairwise") {
        return compare_pairwise(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("benchmark-import-qmsum") {
        return import_qmsum(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("benchmark-run-compact-reference-acp") {
        return run_compact_reference_acp(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("benchmark-score-compact-reference") {
        return score_compact_reference(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("benchmark-import-locomo-memory") {
        return import_locomo_memory(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("benchmark-import-memory-agent-bench") {
        return import_memory_agent_bench(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("benchmark-score-memory-agent-bench") {
        return score_memory_agent_bench(&args[1..]);
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
    let (argv, env) = match acp_launch(argv_json) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("{error}");
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

fn acp_launch(argv_json: &str) -> Result<AcpLaunchArgs, String> {
    let argv: Vec<String> = serde_json::from_str(argv_json)
        .map_err(|error| format!("invalid ACP argv JSON: {error}"))?;
    if argv.is_empty() {
        return Err("ACP argv must not be empty".into());
    }
    let env = match std::env::var("AWAKEN_EVAL_ACP_ENV_JSON") {
        Ok(value) => serde_json::from_str::<std::collections::BTreeMap<String, String>>(&value)
            .map_err(|error| format!("invalid AWAKEN_EVAL_ACP_ENV_JSON: {error}"))?
            .into_iter()
            .collect(),
        Err(std::env::VarError::NotPresent) => Vec::new(),
        Err(error) => return Err(format!("failed to read AWAKEN_EVAL_ACP_ENV_JSON: {error}")),
    };
    Ok((argv, env))
}

async fn run_compact_acp(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path, argv_json] = args else {
        eprintln!("usage: awaken-eval compact-run-acp <dataset.json> <artifact.json> <argv-json>");
        return ExitCode::from(2);
    };
    let dataset = match awaken_eval::store::load_compact_dataset(&PathBuf::from(dataset_path)) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to load compact dataset: {error}");
            return ExitCode::from(2);
        }
    };
    let (argv, env) = match acp_launch(argv_json) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let observations = awaken_eval::compact_eval::run_acp(&dataset, argv, env).await;
    finish_compact(&dataset, &observations, Some(artifact_path))
}

fn score_compact(args: &[String]) -> ExitCode {
    let [dataset_path, observations_path] = args else {
        eprintln!("usage: awaken-eval compact-score <dataset.json> <observations.json>");
        return ExitCode::from(2);
    };
    let dataset = match awaken_eval::store::load_compact_dataset(&PathBuf::from(dataset_path)) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to load compact dataset: {error}");
            return ExitCode::from(2);
        }
    };
    let observations =
        match awaken_eval::store::load_compact_observations(&PathBuf::from(observations_path)) {
            Ok(observations) => observations,
            Err(error) => {
                eprintln!("failed to load compact observations: {error}");
                return ExitCode::from(2);
            }
        };
    finish_compact(&dataset, &observations, None)
}

fn finish_compact(
    dataset: &awaken_eval::compact_eval::CompactDataset,
    observations: &[awaken_eval::compact_eval::CompactObservation],
    artifact_path: Option<&str>,
) -> ExitCode {
    if let Some(path) = artifact_path
        && let Err(error) = save_json(path, observations)
    {
        eprintln!("failed to save compact artifact: {error}");
        return ExitCode::from(2);
    }
    let report = awaken_eval::compact_eval::score(dataset, observations);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if report.exact_cases == report.total_cases {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn run_memory_acp(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path, argv_json] = args else {
        eprintln!("usage: awaken-eval memory-run-acp <dataset.json> <artifact.json> <argv-json>");
        return ExitCode::from(2);
    };
    let dataset = match awaken_eval::store::load_memory_dataset(&PathBuf::from(dataset_path)) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to load memory dataset: {error}");
            return ExitCode::from(2);
        }
    };
    let (argv, env) = match acp_launch(argv_json) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let observations = awaken_eval::memory_eval::run_acp(&dataset, argv, env).await;
    finish_memory(&dataset, &observations, Some(artifact_path))
}

fn score_memory(args: &[String]) -> ExitCode {
    let [dataset_path, observations_path] = args else {
        eprintln!("usage: awaken-eval memory-score <dataset.json> <observations.json>");
        return ExitCode::from(2);
    };
    let dataset = match awaken_eval::store::load_memory_dataset(&PathBuf::from(dataset_path)) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to load memory dataset: {error}");
            return ExitCode::from(2);
        }
    };
    let observations =
        match awaken_eval::store::load_memory_observations(&PathBuf::from(observations_path)) {
            Ok(observations) => observations,
            Err(error) => {
                eprintln!("failed to load memory observations: {error}");
                return ExitCode::from(2);
            }
        };
    finish_memory(&dataset, &observations, None)
}

fn finish_memory(
    dataset: &awaken_eval::memory_eval::MemoryDataset,
    observations: &[awaken_eval::memory_eval::MemoryObservation],
    artifact_path: Option<&str>,
) -> ExitCode {
    if let Some(path) = artifact_path
        && let Err(error) = save_json(path, observations)
    {
        eprintln!("failed to save memory artifact: {error}");
        return ExitCode::from(2);
    }
    let report = awaken_eval::memory_eval::score(dataset, observations);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if report.extraction_exact == report.extraction_total
        && report.selection_exact == report.selection_total
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn save_json(path: &str, value: &(impl serde::Serialize + ?Sized)) -> std::io::Result<()> {
    let data = serde_json::to_string_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    std::fs::write(path, data)
}

fn load_json<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let data = if path == "-" {
        let mut data = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut data)
            .map_err(|error| format!("read stdin: {error}"))?;
        data
    } else {
        std::fs::read_to_string(path).map_err(|error| format!("read {path}: {error}"))?
    };
    serde_json::from_str(&data).map_err(|error| format!("parse {path}: {error}"))
}

fn parse_limit(value: Option<&String>) -> Result<usize, String> {
    value.map_or(Ok(0), |value| {
        value
            .parse::<usize>()
            .map_err(|error| format!("invalid limit {value:?}: {error}"))
    })
}

fn import_rewardbench2(args: &[String]) -> ExitCode {
    let [input_path, output_path, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-import-rewardbench2 <datasets-server-rows.json> <dataset.json> [limit]"
        );
        return ExitCode::from(2);
    };
    if rest.len() > 1 {
        eprintln!("benchmark-import-rewardbench2 accepts at most one limit");
        return ExitCode::from(2);
    }
    let value = match load_json(input_path) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let limit = match parse_limit(rest.first()) {
        Ok(limit) => limit,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let dataset = match awaken_eval::public_benchmark::import_rewardbench2_rows(&value, limit) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to import RewardBench 2: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = save_json(output_path, &dataset) {
        eprintln!("failed to save pairwise dataset: {error}");
        return ExitCode::from(2);
    }
    eprintln!("imported {} RewardBench 2 comparisons", dataset.cases.len());
    ExitCode::SUCCESS
}

async fn run_pairwise_acp(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path, argv_json] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-run-pairwise-acp <dataset.json> <artifact.json> <argv-json>"
        );
        return ExitCode::from(2);
    };
    let dataset: awaken_eval::public_benchmark::PairwiseDataset = match load_json(dataset_path) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = dataset.validate() {
        eprintln!("invalid pairwise dataset: {error}");
        return ExitCode::from(2);
    }
    let (argv, env) = match acp_launch(argv_json) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let observations = awaken_eval::public_benchmark::run_pairwise_acp(&dataset, argv, env).await;
    if let Err(error) = save_json(artifact_path, &observations) {
        eprintln!("failed to save pairwise observations: {error}");
        return ExitCode::from(2);
    }
    print_pairwise(&dataset, &observations)
}

fn score_pairwise(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path] = args else {
        eprintln!("usage: awaken-eval benchmark-score-pairwise <dataset.json> <artifact.json>");
        return ExitCode::from(2);
    };
    let dataset = match load_json(dataset_path) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let observations: Vec<awaken_eval::public_benchmark::BenchmarkObservation> =
        match load_json(artifact_path) {
            Ok(observations) => observations,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        };
    print_pairwise(&dataset, &observations)
}

fn print_pairwise(
    dataset: &awaken_eval::public_benchmark::PairwiseDataset,
    observations: &[awaken_eval::public_benchmark::BenchmarkObservation],
) -> ExitCode {
    let report = awaken_eval::public_benchmark::score_pairwise(dataset, observations);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if report.observed == report.total && report.schema_valid.correct == report.total {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn compare_pairwise(args: &[String]) -> ExitCode {
    let [dataset_path, left_path, right_path] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-compare-pairwise <dataset.json> <left-observations.json> <right-observations.json>"
        );
        return ExitCode::from(2);
    };
    let dataset = match load_json(dataset_path) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let left: Vec<awaken_eval::public_benchmark::BenchmarkObservation> = match load_json(left_path)
    {
        Ok(observations) => observations,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let right: Vec<awaken_eval::public_benchmark::BenchmarkObservation> =
        match load_json(right_path) {
            Ok(observations) => observations,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        };
    let report = awaken_eval::public_benchmark::compare_pairwise(&dataset, &left, &right);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if report.jointly_valid == report.total {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn import_qmsum(args: &[String]) -> ExitCode {
    let [input_dir, output_path, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-import-qmsum <json-directory> <dataset.json> [limit]"
        );
        return ExitCode::from(2);
    };
    if rest.len() > 1 {
        eprintln!("benchmark-import-qmsum accepts at most one limit");
        return ExitCode::from(2);
    }
    let limit = match parse_limit(rest.first()) {
        Ok(limit) => limit,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let entries = match std::fs::read_dir(input_dir) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("read QMSum directory: {error}");
            return ExitCode::from(2);
        }
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut documents = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(path) = path.to_str() else {
            eprintln!("non-UTF8 QMSum path");
            return ExitCode::from(2);
        };
        match load_json(path) {
            Ok(value) => documents.push(value),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        }
    }
    let dataset = match awaken_eval::public_benchmark::import_qmsum_documents(&documents, limit) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to import QMSum: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = save_json(output_path, &dataset) {
        eprintln!("failed to save QMSum dataset: {error}");
        return ExitCode::from(2);
    }
    eprintln!("imported {} QMSum cases", dataset.cases.len());
    ExitCode::SUCCESS
}

async fn run_compact_reference_acp(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path, argv_json] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-run-compact-reference-acp <dataset.json> <artifact.json> <argv-json>"
        );
        return ExitCode::from(2);
    };
    let dataset: awaken_eval::public_benchmark::ReferenceCompactDataset =
        match load_json(dataset_path) {
            Ok(dataset) => dataset,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        };
    if let Err(error) = dataset.validate() {
        eprintln!("invalid reference compact dataset: {error}");
        return ExitCode::from(2);
    }
    let (argv, env) = match acp_launch(argv_json) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let observations =
        awaken_eval::public_benchmark::run_reference_compact_acp(&dataset, argv, env).await;
    if let Err(error) = save_json(artifact_path, &observations) {
        eprintln!("failed to save compact observations: {error}");
        return ExitCode::from(2);
    }
    print_compact_reference(&dataset, &observations)
}

fn score_compact_reference(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-score-compact-reference <dataset.json> <artifact.json>"
        );
        return ExitCode::from(2);
    };
    let dataset = match load_json(dataset_path) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let observations: Vec<awaken_eval::public_benchmark::BenchmarkObservation> =
        match load_json(artifact_path) {
            Ok(observations) => observations,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        };
    print_compact_reference(&dataset, &observations)
}

fn print_compact_reference(
    dataset: &awaken_eval::public_benchmark::ReferenceCompactDataset,
    observations: &[awaken_eval::public_benchmark::BenchmarkObservation],
) -> ExitCode {
    let report = awaken_eval::public_benchmark::score_reference_compact(dataset, observations);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if report.observed == report.total {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn import_locomo_memory(args: &[String]) -> ExitCode {
    let [input_path, output_path, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-import-locomo-memory <locomo.json> <memory-dataset.json> [limit] [distractors]"
        );
        return ExitCode::from(2);
    };
    if rest.len() > 2 {
        eprintln!("benchmark-import-locomo-memory accepts limit and distractors");
        return ExitCode::from(2);
    }
    let limit = match parse_limit(rest.first()) {
        Ok(limit) => limit,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let distractors = match rest.get(1).map_or(Ok(9), |value| {
        value
            .parse::<usize>()
            .map_err(|error| format!("invalid distractors: {error}"))
    }) {
        Ok(value) if value > 0 => value,
        Ok(_) => {
            eprintln!("distractors must be positive");
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let value = match load_json(input_path) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let dataset =
        match awaken_eval::public_benchmark::import_locomo_selection(&value, limit, distractors) {
            Ok(dataset) => dataset,
            Err(error) => {
                eprintln!("failed to import LoCoMo: {error}");
                return ExitCode::from(2);
            }
        };
    if let Err(error) = save_json(output_path, &dataset) {
        eprintln!("failed to save LoCoMo dataset: {error}");
        return ExitCode::from(2);
    }
    eprintln!(
        "imported {} LoCoMo selector cases",
        dataset.selection_cases.len()
    );
    ExitCode::SUCCESS
}

fn import_memory_agent_bench(args: &[String]) -> ExitCode {
    let [category, input_path, output_path, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-import-memory-agent-bench <category> <rows.json> <dataset.json> [limit]"
        );
        return ExitCode::from(2);
    };
    if rest.len() > 1 {
        eprintln!("benchmark-import-memory-agent-bench accepts at most one limit");
        return ExitCode::from(2);
    }
    let category = match awaken_eval::memory_agent_bench::MemoryAgentBenchCategory::parse(category)
    {
        Ok(category) => category,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let limit = match parse_limit(rest.first()) {
        Ok(limit) => limit,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let value = match load_json(input_path) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let dataset = match awaken_eval::memory_agent_bench::import_rows(&value, category, limit) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("failed to import MemoryAgentBench: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = save_json(output_path, &dataset) {
        eprintln!("failed to save MemoryAgentBench dataset: {error}");
        return ExitCode::from(2);
    }
    eprintln!(
        "imported {} MemoryAgentBench cases / {} questions",
        dataset.cases.len(),
        dataset.question_count()
    );
    ExitCode::SUCCESS
}

fn score_memory_agent_bench(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path] = args else {
        eprintln!(
            "usage: awaken-eval benchmark-score-memory-agent-bench <dataset.json> <observations.json>"
        );
        return ExitCode::from(2);
    };
    let dataset: awaken_eval::memory_agent_bench::MemoryAgentBenchDataset =
        match load_json(dataset_path) {
            Ok(dataset) => dataset,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        };
    if let Err(error) = dataset.validate() {
        eprintln!("invalid MemoryAgentBench dataset: {error}");
        return ExitCode::from(2);
    }
    let observations: Vec<awaken_eval::memory_agent_bench::MemoryAgentBenchObservation> =
        match load_json(artifact_path) {
            Ok(observations) => observations,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        };
    let report = awaken_eval::memory_agent_bench::score(&dataset, &observations);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if report.observed == report.total
        && report.duplicate_observation_ids.is_empty()
        && report.unexpected_observation_ids.is_empty()
    {
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
