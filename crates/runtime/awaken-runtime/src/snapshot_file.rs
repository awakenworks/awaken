use std::io::{Error, ErrorKind, Result};
use std::path::Path;

use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

use crate::Runtime;

impl Runtime {
    /// Load the canonical snapshot JSON directly for embedded execution. Local
    /// model edits receive a fresh content identity before the regular run path.
    pub fn load_snapshot_file(&self, path: impl AsRef<Path>) -> Result<ExecutableAgentSnapshot> {
        let reader = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut snapshot: ExecutableAgentSnapshot =
            serde_json::from_reader(reader).map_err(invalid_data)?;
        snapshot.validate_embedded_native().map_err(invalid_data)?;
        snapshot.recompute_fingerprint().map_err(invalid_data)?;
        self.validate_plugins(&snapshot.resolved_spec)
            .map_err(invalid_data)?;
        Ok(snapshot)
    }

    /// Persist the same snapshot contract consumed by [`Runtime::run`].
    pub fn save_snapshot_file(
        snapshot: &ExecutableAgentSnapshot,
        path: impl AsRef<Path>,
    ) -> Result<()> {
        snapshot.validate_embedded_native().map_err(invalid_data)?;
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        let mut bytes = serde_json::to_vec_pretty(snapshot).map_err(invalid_data)?;
        bytes.push(b'\n');
        std::io::copy(&mut bytes.as_slice(), temporary.as_file_mut())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
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
        let runtime = Runtime::new();
        let loaded = runtime.load_snapshot_file(&path).unwrap();
        assert_eq!(
            loaded.resolved_spec.model_binding.binding().model_ref,
            "model-b"
        );
        assert_ne!(loaded.fingerprint, snapshot.fingerprint);
        assert_eq!(loaded.fingerprint, loaded.resolved_spec.catalog_fingerprint);

        value["resolved_spec"]["model_binding"]["backend_ref"] = "acp:claude".into();
        serde_json::to_writer_pretty(std::fs::File::create(&path).unwrap(), &value).unwrap();
        assert_eq!(
            runtime.load_snapshot_file(path).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    /// Cause/effect decision table: R1 a rejected save must preserve the prior
    /// complete file; R2 a selected but uninstalled plugin must fail during load
    /// before execution; R3 a valid replacement is atomically loadable and leaves
    /// no sibling temporary artifact.
    #[test]
    fn snapshot_file_replacement_and_embedded_preflight_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent.snapshot.json");
        let original = ExecutableAgentSnapshot::builder("agent-a")
            .model(ModelBinding {
                provider_identity_ref: "local".into(),
                model_ref: "model-a".into(),
                backend_ref: "genai".into(),
            })
            .build();
        Runtime::save_snapshot_file(&original, &path).unwrap();
        let original_bytes = std::fs::read(&path).unwrap();

        let mut rejected = original.clone();
        let mut binding = rejected.resolved_spec.model_binding.binding().clone();
        binding.backend_ref = "a2a:https://agent".into();
        rejected.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);
        assert_eq!(
            Runtime::save_snapshot_file(&rejected, &path)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes, "R1");

        let mut missing_plugin = original.clone();
        missing_plugin.resolved_spec.plugin_ids = vec!["missing".into()];
        Runtime::save_snapshot_file(&missing_plugin, &path).unwrap();
        let error = Runtime::new().load_snapshot_file(&path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData, "R2");
        assert!(error.to_string().contains("missing"), "R2");

        Runtime::save_snapshot_file(&original, &path).unwrap();
        Runtime::new().load_snapshot_file(&path).unwrap();
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "R3"
        );
    }
}
