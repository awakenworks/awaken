//! Persist datasets as JSON files. Fixtures are plain data (no per-subject
//! content), so this is a straight serde round-trip; a record-from-real-run path
//! that captures `Purpose::EvalRecording` content attaches an erasure fan-out.

use std::path::Path;

use crate::Dataset;

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
