//! CLI adapter for Admin Assistant authoring evaluation.

use std::collections::BTreeSet;
use std::process::ExitCode;

use awaken_eval::admin_authoring::{
    self, AdminAuthoringDataset, AdminAuthoringFloors, AdminAuthoringObservation,
};

use super::{load_json, save_json};

pub async fn run_live(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval admin-authoring-run-live <dataset.json> <observations.json> [base-url] [repetitions] [case-id,...]"
        );
        return ExitCode::from(2);
    };
    if rest.len() > 3 {
        eprintln!("admin-authoring-run-live accepts base-url, repetitions, and case ids");
        return ExitCode::from(2);
    }
    let dataset: AdminAuthoringDataset = match load_json(dataset_path) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = dataset.validate() {
        eprintln!("invalid Admin authoring dataset: {error}");
        return ExitCode::from(2);
    }
    let base_url = rest
        .first()
        .map_or("http://127.0.0.1:38080", String::as_str);
    let repetitions = match rest.get(1).map_or(Ok(3), |value| value.parse::<usize>()) {
        Ok(value) if value > 0 => value,
        Ok(_) => {
            eprintln!("repetitions must be positive");
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("invalid repetitions: {error}");
            return ExitCode::from(2);
        }
    };
    let selected = rest.get(2).map(|value| {
        value
            .split(',')
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    });
    let observations =
        admin_authoring::run_live(&dataset, base_url, repetitions, selected.as_ref()).await;
    if let Err(error) = save_json(artifact_path, &observations) {
        eprintln!("failed to save Admin authoring observations: {error}");
        return ExitCode::from(2);
    }
    let failures = observations
        .iter()
        .filter(|observation| observation.error.is_some())
        .count();
    eprintln!(
        "recorded {} Admin authoring observations ({failures} execution failures)",
        observations.len()
    );
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub fn score(args: &[String]) -> ExitCode {
    let [dataset_path, artifact_path, rest @ ..] = args else {
        eprintln!(
            "usage: awaken-eval admin-authoring-score <dataset.json> <observations.json> [min-criteria] [min-fully-correct] [min-persisted]"
        );
        return ExitCode::from(2);
    };
    if rest.len() > 3 {
        eprintln!("admin-authoring-score accepts at most three quality floors");
        return ExitCode::from(2);
    }
    let dataset: AdminAuthoringDataset = match load_json(dataset_path) {
        Ok(dataset) => dataset,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = dataset.validate() {
        eprintln!("invalid Admin authoring dataset: {error}");
        return ExitCode::from(2);
    }
    let observations: Vec<AdminAuthoringObservation> = match load_json(artifact_path) {
        Ok(observations) => observations,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let defaults = AdminAuthoringFloors::default();
    let parse_floor = |index: usize, default: f64| {
        rest.get(index).map_or(Ok(default), |value| {
            value
                .parse::<f64>()
                .map_err(|error| format!("invalid quality floor {value:?}: {error}"))
        })
    };
    let floors = match (
        parse_floor(0, defaults.criteria),
        parse_floor(1, defaults.fully_correct),
        parse_floor(2, defaults.persisted),
    ) {
        (Ok(criteria), Ok(fully_correct), Ok(persisted)) => AdminAuthoringFloors {
            criteria,
            fully_correct,
            persisted,
        }
        .validate(),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => Err(error),
    };
    let floors = match floors {
        Ok(floors) => floors,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let report = admin_authoring::score(&dataset, &observations, floors);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if report.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
