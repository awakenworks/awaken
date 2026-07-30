use std::io::{Error, ErrorKind, Result};
use std::path::Path;

use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

use crate::Runtime;

impl Runtime {
    /// Load the canonical snapshot JSON directly for embedded execution. Local
    /// model edits receive a fresh content identity before the regular run path.
    pub fn load_snapshot_file(path: impl AsRef<Path>) -> Result<ExecutableAgentSnapshot> {
        let reader = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut snapshot: ExecutableAgentSnapshot =
            serde_json::from_reader(reader).map_err(invalid_data)?;
        snapshot.validate_embedded_native().map_err(invalid_data)?;
        snapshot.recompute_fingerprint().map_err(invalid_data)?;
        Ok(snapshot)
    }

    /// Persist the same snapshot contract consumed by [`Runtime::run`].
    pub fn save_snapshot_file(
        snapshot: &ExecutableAgentSnapshot,
        path: impl AsRef<Path>,
    ) -> Result<()> {
        snapshot.validate_embedded_native().map_err(invalid_data)?;
        let writer = std::io::BufWriter::new(std::fs::File::create(path)?);
        serde_json::to_writer_pretty(writer, snapshot).map_err(invalid_data)
    }
}

fn invalid_data(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    use super::*;

    /// Cause/effect decision table: R1 valid native JSON plus a model edit =>
    /// load succeeds and all fingerprint fields are re-derived; R2 an ACP edit
    /// => InvalidData before execution; R3 save+load without edit => same agent.
    #[test]
    fn snapshot_file_is_the_offline_sdk_contract() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent.snapshot.json");
        let snapshot = ExecutableAgentSnapshot::builder("agent-a")
            .model(ModelBinding {
                provider_identity_ref: "local".into(),
                model_ref: "model-a".into(),
                backend_ref: "genai".into(),
            })
            .build();
        Runtime::save_snapshot_file(&snapshot, &path).unwrap();

        let mut value: serde_json::Value =
            serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
        value["resolved_spec"]["model_binding"]["model_ref"] = "model-b".into();
        serde_json::to_writer_pretty(std::fs::File::create(&path).unwrap(), &value).unwrap();
        let loaded = Runtime::load_snapshot_file(&path).unwrap();
        assert_eq!(
            loaded.resolved_spec.model_binding.binding.model_ref,
            "model-b"
        );
        assert_ne!(loaded.fingerprint, snapshot.fingerprint);
        assert_eq!(loaded.fingerprint, loaded.resolved_spec.catalog_fingerprint);

        value["resolved_spec"]["model_binding"]["backend_ref"] = "acp:claude".into();
        serde_json::to_writer_pretty(std::fs::File::create(&path).unwrap(), &value).unwrap();
        assert_eq!(
            Runtime::load_snapshot_file(path).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }
}
