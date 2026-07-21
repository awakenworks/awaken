//! Persist datasets as JSON files. Fixtures are plain data (no per-subject
//! content), so this is a straight serde round-trip; a record-from-real-run path
//! that captures `Purpose::EvalRecording` content attaches an erasure fan-out.

use std::path::Path;

use crate::Dataset;
use crate::outcome_judge::{JudgeDataset, JudgeObservation};

/// Read a dataset from a JSON file.
pub fn load_dataset(path: &Path) -> std::io::Result<Dataset> {
    let data = std::fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write a dataset to a JSON file (pretty-printed).
pub fn save_dataset(path: &Path, dataset: &Dataset) -> std::io::Result<()> {
    let data = serde_json::to_string_pretty(dataset)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, data)
}

/// Read and validate a versioned Outcome Judge dataset.
pub fn load_judge_dataset(path: &Path) -> std::io::Result<JudgeDataset> {
    let data = std::fs::read_to_string(path)?;
    let dataset: JudgeDataset = serde_json::from_str(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    dataset
        .validate()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(dataset)
}

/// Read provider outputs recorded separately from the frozen Judge corpus.
pub fn load_judge_observations(path: &Path) -> std::io::Result<Vec<JudgeObservation>> {
    let data = std::fs::read_to_string(path)?;
    if let Ok(observations) = serde_json::from_str(&data) {
        return Ok(observations);
    }
    let value: serde_json::Value = serde_json::from_str(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    serde_json::from_value(
        value
            .get("observations")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write a frozen or locally-derived Outcome Judge dataset.
pub fn save_judge_dataset(path: &Path, dataset: &JudgeDataset) -> std::io::Result<()> {
    dataset
        .validate()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let data = serde_json::to_string_pretty(dataset)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, data)
}
