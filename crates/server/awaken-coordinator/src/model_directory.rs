//! Workspace-aware Managed `/v1/models` projection.

use std::sync::Arc;

use awaken_executable_agent_contract::ExecutableAgentInventorySource;
use awaken_protocol_managed::{ModelDirectory, ModelDirectoryFuture, ModelEntry};

/// Coordinator-native model projection derived from the same immutable
/// executable registrations used for Session resolution.
pub struct ExecutableAgentModelDirectory {
    registrations: Arc<dyn ExecutableAgentInventorySource>,
}

impl ExecutableAgentModelDirectory {
    #[must_use]
    pub fn new(registrations: Arc<dyn ExecutableAgentInventorySource>) -> Self {
        Self { registrations }
    }
}

impl ModelDirectory for ExecutableAgentModelDirectory {
    fn list<'a>(&'a self, workspace_id: &'a str) -> ModelDirectoryFuture<'a> {
        Box::pin(async move {
            let registrations = self
                .registrations
                .current_registrations(workspace_id)
                .await
                .map_err(|error| error.to_string())?;
            let mut entries = registrations
                .into_iter()
                .flat_map(|registration| {
                    let spec = registration.snapshot.resolved_spec;
                    std::iter::once(spec.model_binding)
                        .chain(spec.model_candidates)
                        .map(|candidate| candidate.binding.model_ref)
                })
                .filter(|model_ref| !model_ref.trim().is_empty())
                .map(|model_ref| ModelEntry::new(&model_ref, &model_ref))
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.id.cmp(&right.id));
            entries.dedup_by(|left, right| left.id == right.id);
            Ok(entries)
        })
    }
}

/// The sole built-in executor-to-model capability projection. Runtime
/// diagnostics, model discovery, and publication consume this data rather than
/// rebuilding CLI-specific dialect tables.
#[must_use]
pub fn installed_executor_model_capabilities()
-> Vec<awaken_config_resolver::ExecutorModelCapability> {
    std::iter::once(awaken_config_resolver::ExecutorModelCapability::native())
        .chain(
            awaken_run_executor_acp::known_acp_clis()
                .iter()
                .map(|executor| awaken_config_resolver::ExecutorModelCapability {
                    backend_ref: format!("acp:{}", executor.id),
                    model_api_dialects: executor
                        .model_api_dialects
                        .iter()
                        .map(|dialect| (*dialect).to_string())
                        .collect(),
                    available: true,
                }),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_executable_agent_contract::{
        ExecutableAgentRegistration, ExecutableAgentRegistrationError,
        ExecutableAgentSessionProfile,
    };
    use awaken_runtime_contract::resolved::ModelBinding;

    struct Inventory(Vec<ExecutableAgentRegistration>);

    #[async_trait::async_trait]
    impl ExecutableAgentInventorySource for Inventory {
        async fn current_registrations(
            &self,
            workspace_id: &str,
        ) -> Result<Vec<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
            Ok(self
                .0
                .iter()
                .filter(|registration| registration.workspace_id == workspace_id)
                .cloned()
                .collect())
        }
    }

    fn registration(agent: &str, primary: &str, fallbacks: &[&str]) -> ExecutableAgentRegistration {
        ExecutableAgentRegistration {
            workspace_id: "workspace-a".into(),
            agent_id: agent.into(),
            source_revision: 1,
            snapshot: awaken_runtime_contract::ExecutableAgentSnapshot::builder(agent)
                .model(ModelBinding::new("provider", primary, "native"))
                .model_candidates(
                    fallbacks
                        .iter()
                        .map(|model| ModelBinding::new("provider", *model, "native")),
                )
                .build(),
            session_profile: ExecutableAgentSessionProfile::default(),
        }
    }

    #[tokio::test]
    async fn model_directory_projects_current_registration_inventory() {
        // Causes: C1 registration belongs to the requested Workspace; C2 the
        // same model ref occurs in multiple current registrations; C3 no current
        // registration belongs to the requested Workspace. Effects: E1 project
        // primary and fallback refs; E2 sort and deduplicate them; E3 return an
        // empty directory. Decision rules: R1=C1&&!C2 -> E1; R2=C1&&C2 ->
        // E1+E2; R3=C3 -> E3. This inventory is the sole runtime source, so no
        // Control catalog fallback is part of any rule.
        let inventory = Arc::new(Inventory(vec![
            registration("agent-a", "model-b", &["model-a"]),
            registration("agent-b", "model-a", &["model-c"]),
        ]));
        let directory = ExecutableAgentModelDirectory::new(inventory);
        let entries = directory.list("workspace-a").await.unwrap();
        assert_eq!(
            entries
                .into_iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec!["model-a", "model-b", "model-c"],
            "R1/R2"
        );
        assert!(
            directory.list("workspace-b").await.unwrap().is_empty(),
            "R3"
        );
    }
}
