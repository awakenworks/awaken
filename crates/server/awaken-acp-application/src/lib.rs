// @formal entrypoint worker_credential.host_observation_projection_is_backend_scoped
// @formal entrypoint worker_credential.local_binding_batch_is_atomic
//! Coordinator ACP registration and secret-free projection over supplied catalog facts.
//! Host discovery, installation and resolver assembly belong to CLI/Worker composition.
//!
//! Type-ownership rule A3: the registration app cannot name a physical host
//! preparation adapter. The existing CLI imports it from its actual owner.
//!
//! ```compile_fail
//! use awaken_acp_application::HostAcpAdapters;
//! ```

use awaken_acp_contract::{AcpLocalCredentialBinding, AcpPublicationCapability};
use awaken_credential_contract::CredentialRef;
use awaken_credential_vault::repo::{CredentialRepo, ensure_worker_local};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, WorkerLocalBinding,
};
use awaken_runtime_contract::resolved::Backend;

/// Idempotently create the non-secret WorkerLocal identities requested by one
/// execution Workspace. Discovery is deliberately not required here: the
/// control plane records intent, while Worker observations independently prove
/// whether a matching host identity is currently usable.
pub async fn ensure_workspace_bindings<'a>(
    credentials: &dyn CredentialRepo,
    workspace: &str,
    cli_ids: impl IntoIterator<Item = &'a str>,
    catalog: &[AcpPublicationCapability],
) -> Result<(), String> {
    for cli_id in cli_ids {
        let backend_ref = format!("acp:{cli_id}");
        if !catalog.iter().any(|row| row.backend_ref == backend_ref) {
            return Err(format!("local ACP binding names unknown CLI `{cli_id}`"));
        }
        ensure_worker_local(
            credentials,
            workspace,
            WorkerLocalBinding::new(backend_ref, "default"),
            None,
        )
        .await
        .map_err(|error| format!("register local ACP binding for {cli_id}: {error}"))?;
    }
    Ok(())
}

/// Project Control source metadata once; host adapters only accept these exact bindings.
pub fn project_local_credential_sources(
    catalog: &[AcpPublicationCapability],
    sources: impl IntoIterator<Item = CredentialSource>,
) -> Result<Vec<AcpLocalCredentialBinding>, String> {
    let mut projected = Vec::new();
    for source in sources {
        if source.kind != CredentialKind::WorkerLocal {
            continue;
        }
        let binding = source
            .worker_local_binding
            .as_ref()
            .ok_or("WorkerLocal credential requires a binding")?;
        let cli_id = match Backend::from_ref(&binding.driver_id) {
            Backend::Acp(backend) => backend.cli().to_string(),
            Backend::Invalid(_) => return Err("invalid WorkerLocal driver".into()),
            _ => continue,
        };
        if binding.subject_id != "default" {
            return Err("host ACP supports only the default local subject".into());
        }
        if source.material_ref.is_some()
            || !source.auxiliary_material_refs.is_empty()
            || source.oauth_command.is_some()
            || source.env_key.is_some()
        {
            return Err("WorkerLocal ACP binding must not contain material locators".into());
        }
        let projection = AcpLocalCredentialBinding {
            credential: CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version)
                    .map_err(|_| "WorkerLocal revision must be positive")?,
            },
            cli_id,
            enabled: source.status == CredentialStatus::Active,
        };
        projection.validate(catalog)?;
        projected.push(projection);
    }
    Ok(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_acp_contract::AcpModelSelectionSupport;
    use awaken_credential_vault::repo::InMemoryCredentialRepo;

    fn publication(cli: &str) -> AcpPublicationCapability {
        // Test input, not a production catalog or discovery registry.
        AcpPublicationCapability {
            supports_tool_aliases: false,
            backend_ref: format!("acp:{cli}"),
            model_api_dialects: Vec::new(),
            model_selection: AcpModelSelectionSupport::DefaultOnly,
            model_delivery_credential_environments: None,
        }
    }

    #[tokio::test]
    async fn workspace_registration_consumes_only_the_supplied_publication_view() {
        // NI2-A2 causes: C1 CLI is published; C2 repeat; C3 later unknown row.
        // R1 C1 -> one non-secret default identity; R2 C1+C2 -> same id/revision;
        // R3 !C1 -> reject before write; R4 C1 then C3 -> original per-row
        // idempotent transaction, retry adds no duplicate. No host port exists.
        let repo = InMemoryCredentialRepo::new();
        let catalog = [publication("fixture")];
        ensure_workspace_bindings(&repo, "w", ["unknown"], &catalog)
            .await
            .unwrap_err();
        assert!(repo.list("w").await.unwrap().is_empty(), "R3");
        ensure_workspace_bindings(&repo, "w", ["fixture"], &catalog)
            .await
            .unwrap();
        let first = repo.list("w").await.unwrap();
        assert_eq!(first.len(), 1, "R1");
        ensure_workspace_bindings(&repo, "w", ["fixture"], &catalog)
            .await
            .unwrap();
        let repeated = repo.list("w").await.unwrap();
        assert_eq!(repeated.len(), 1, "R2");
        assert_eq!(repeated[0].id, first[0].id, "R2");
        assert_eq!(repeated[0].version, first[0].version, "R2");
        ensure_workspace_bindings(&repo, "w", ["fixture", "unknown"], &catalog)
            .await
            .unwrap_err();
        let after = repo.list("w").await.unwrap();
        assert_eq!(after.len(), 1, "R4");
        assert_eq!(after[0].id, first[0].id, "R4");
        assert_eq!(after[0].version, first[0].version, "R4");
    }

    #[tokio::test]
    async fn control_projection_rejects_unsupported_subjects_and_material_locators() {
        // P1 active/disabled/archived default-account metadata maps to exactly
        // one enabled bit and no material fields. P2 non-default subject,
        // material/environment/helper locators or malformed driver/revision
        // reject the complete projection; no host login can stand in for a
        // different authored account. P3 unrelated non-ACP sources are ignored.
        // NI2-A1: the supplied publication is metadata, not discovery.
        // All original status/subject/material P1/P2/P3 rules remain below.
        let catalog = [publication("codex")];
        let repo = InMemoryCredentialRepo::new();
        let source = ensure_worker_local(
            &repo,
            "workspace",
            WorkerLocalBinding::new("acp:codex", "default"),
            None,
        )
        .await
        .unwrap();
        for status in [
            CredentialStatus::Active,
            CredentialStatus::Disabled,
            CredentialStatus::Archived,
        ] {
            let mut row = source.clone();
            row.status = status;
            let result = project_local_credential_sources(&catalog, [row]).unwrap();
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].enabled, status == CredentialStatus::Active);
            assert_eq!(result[0].cli_id, "codex");
            assert_eq!(result[0].credential.id, source.id.0);
        }
        for fault in [
            "subject",
            "env",
            "helper",
            "driver",
            "material",
            "auxiliary",
        ] {
            let mut row = source.clone();
            match fault {
                "subject" => {
                    row.worker_local_binding.as_mut().unwrap().subject_id = "secondary".into()
                }
                "env" => row.env_key = Some("FIXTURE_KEY".into()),
                "helper" => row.oauth_command = Some(vec!["must-not-run".into()]),
                "driver" => row.worker_local_binding.as_mut().unwrap().driver_id = "acp:".into(),
                "material" => {
                    row.material_ref = Some(awaken_credential_vault::SecretRef("fixture".into()))
                }
                "auxiliary" => {
                    row.auxiliary_material_refs.insert(
                        "fixture".into(),
                        awaken_credential_vault::SecretRef("fixture".into()),
                    );
                }
                _ => unreachable!(),
            }
            assert!(
                project_local_credential_sources(&catalog, [source.clone(), row]).is_err(),
                "P2 {fault}"
            );
        }
        let mut unrelated = source;
        unrelated.worker_local_binding = Some(WorkerLocalBinding::new("extension", "other"));
        assert!(
            project_local_credential_sources(&catalog, [unrelated])
                .unwrap()
                .is_empty(),
            "P3"
        );
    }
}
