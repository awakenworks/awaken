//! Generic write-only Credential material ingress over the canonical Vault.
//!
//! This adapter owns no consumer configuration, material, or source state. It
//! compiles a caller-declared provider/target/usage/material tuple and delegates
//! sealing, idempotent creation, exact-revision rotation, and retirement to the
//! existing Credential WAL/CAS operations implemented by the parent `VaultState`.

use std::collections::BTreeMap;

use awaken_credential_contract::{
    CredentialDescriptor, CredentialMaterialDescriptor, CredentialSourceId,
    CredentialTargetContract, OPAQUE_SECRET_MATERIAL_TYPE,
};
use awaken_credential_vault::repo::{
    CredentialMaterialPatch, CredentialRetirement, revoke_credential_exact,
    rotate_credential_materials_exact,
};
use awaken_credential_vault::{
    CredentialCreateParams as DomainCredentialCreateParams, CredentialKind, CredentialStatus,
};
use awaken_session_application::{
    CredentialMaterialIngress, CredentialMaterialIngressCommand, CredentialMaterialIngressReceipt,
    CredentialMaterialInput, CredentialMaterialRetirementCommand,
    CredentialMaterialRotationCommand, CredentialPlaintext, SessionParticipantProvenance,
};

use super::VaultState;

#[async_trait::async_trait]
impl CredentialMaterialIngress for VaultState {
    async fn enter_material(
        &self,
        command: CredentialMaterialIngressCommand,
    ) -> Result<CredentialMaterialIngressReceipt, String> {
        let CredentialMaterialIngressCommand {
            source_id,
            workspace_id,
            target,
            usage,
            material,
        } = command;
        let (provider, material_descriptor, material) = encode_material(material)?;
        let descriptor = CredentialDescriptor::new(
            provider,
            material_descriptor,
            [CredentialTargetContract::new(target, usage)],
        );
        let entry = awaken_credential_vault::repo::enter_credential_idempotent_described(
            source_id,
            DomainCredentialCreateParams {
                workspace_id: workspace_id.to_string(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(material),
                oauth_command: None,
            },
            None,
            descriptor,
            self.secrets.as_ref(),
            self.repository.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let revision = u64::try_from(entry.source.version)
            .map_err(|_| "credential revision exceeds the Session range".to_string())?;
        Ok(CredentialMaterialIngressReceipt {
            credential: awaken_credential_contract::CredentialRef {
                id: entry.source.id.0,
                revision,
            },
            provenance: if entry.created {
                SessionParticipantProvenance::Applied
            } else {
                SessionParticipantProvenance::Replayed
            },
        })
    }

    async fn rotate_material(
        &self,
        command: CredentialMaterialRotationCommand,
    ) -> Result<u64, String> {
        let CredentialMaterialRotationCommand {
            source_id,
            expected_revision,
            workspace_id,
            target,
            usage,
            material,
        } = command;
        let (provider, material_descriptor, encoded_material) = encode_material(material)?;
        let expected_descriptor = CredentialDescriptor::new(
            provider,
            material_descriptor,
            [CredentialTargetContract::new(target, usage)],
        );
        let current = self
            .repository
            .get(&source_id)
            .await
            .map_err(|error| error.to_string())?;
        if current.workspace_id != workspace_id
            || current.descriptor.as_ref() != Some(&expected_descriptor)
        {
            return Err("credential binding is unavailable in this Workspace".into());
        }
        let expected_version = i64::try_from(expected_revision)
            .map_err(|_| "credential revision exceeds the Vault range".to_string())?;
        let completed_version = expected_version
            .checked_add(1)
            .ok_or_else(|| "credential revision exceeds the Vault range".to_string())?;
        if current.version == completed_version {
            if current.status != CredentialStatus::Active {
                return Err("credential is not active".into());
            }
            let sealed = awaken_credential_vault::materialize(&current, self.secrets.as_ref())
                .await
                .map_err(|error| error.to_string())?;
            if sealed.expose_secret() != encoded_material.expose_secret() {
                return Err("credential revision changed before material rotation".into());
            }
            return u64::try_from(current.version)
                .map_err(|_| "credential revision exceeds the Session range".into());
        }
        if current.version != expected_version {
            return Err("credential revision changed before material rotation".into());
        }
        let rotated = rotate_credential_materials_exact(
            &source_id,
            expected_version,
            CredentialMaterialPatch {
                primary: Some(encoded_material),
                auxiliary: BTreeMap::new(),
                descriptor: None,
            },
            self.secrets.as_ref(),
            self.repository.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        u64::try_from(rotated.version)
            .map_err(|_| "credential revision exceeds the Session range".into())
    }

    async fn retire_material(
        &self,
        command: CredentialMaterialRetirementCommand,
    ) -> Result<(), String> {
        let CredentialMaterialRetirementCommand {
            credential,
            workspace_id,
        } = command;
        let source_id = CredentialSourceId(credential.id);
        let source = self
            .repository
            .get(&source_id)
            .await
            .map_err(|error| error.to_string())?;
        if source.workspace_id != workspace_id {
            return Err("credential binding is unavailable in this Workspace".into());
        }
        let expected = i64::try_from(credential.revision)
            .map_err(|_| "credential revision exceeds the Vault range".to_string())?;
        let completed = expected
            .checked_add(1)
            .ok_or_else(|| "credential revision exceeds the Vault range".to_string())?;
        if source.version == completed
            && source.status == CredentialStatus::Archived
            && source.material_ref.is_none()
            && source.auxiliary_material_refs.is_empty()
        {
            return Ok(());
        }
        revoke_credential_exact(
            &source_id,
            expected,
            CredentialRetirement::Archive,
            self.secrets.as_ref(),
            self.repository.as_ref(),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
}

fn encode_material(
    material: CredentialMaterialInput,
) -> Result<
    (
        String,
        CredentialMaterialDescriptor,
        awaken_agent_contract::RedactedString,
    ),
    String,
> {
    let descriptor = match &material.plaintext {
        CredentialPlaintext::Opaque(_) => {
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE)
        }
        CredentialPlaintext::Structured(structured) => CredentialMaterialDescriptor::structured(
            structured.type_id.clone(),
            structured.fields.keys().cloned(),
        ),
    };
    let encoded = match material.plaintext {
        CredentialPlaintext::Opaque(value) => value,
        CredentialPlaintext::Structured(structured) => {
            awaken_credential_vault::encode_structured_material(structured)
                .map_err(|error| error.to_string())?
        }
    };
    Ok((material.provider, descriptor, encoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_contract::http_basic_material;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
    use awaken_credential_vault::{InMemorySecretStore, SecretStore};

    fn material(provider: &str, token: &str) -> CredentialMaterialInput {
        CredentialMaterialInput::structured(
            provider,
            http_basic_material(
                RedactedString::new("x-access-token"),
                RedactedString::new(token),
            ),
        )
    }

    fn enter(
        source_id: CredentialSourceId,
        workspace_id: &str,
        target: awaken_credential_contract::CredentialTarget,
        provider: &str,
        token: &str,
    ) -> CredentialMaterialIngressCommand {
        CredentialMaterialIngressCommand {
            source_id,
            workspace_id: workspace_id.into(),
            target,
            usage: awaken_session_contract::repository_transport_credential_usage(),
            material: material(provider, token),
        }
    }

    fn rotate(
        source_id: CredentialSourceId,
        expected_revision: u64,
        workspace_id: &str,
        target: awaken_credential_contract::CredentialTarget,
        provider: &str,
        token: &str,
    ) -> CredentialMaterialRotationCommand {
        CredentialMaterialRotationCommand {
            source_id,
            expected_revision,
            workspace_id: workspace_id.into(),
            target,
            usage: awaken_session_contract::repository_transport_credential_usage(),
            material: material(provider, token),
        }
    }

    /// Generic ingress cause/effect graph: C1 entry source is absent; C2 an
    /// existing source descriptor matches exactly; C3 rotation Workspace and
    /// descriptor match exactly; C4 the existing row has a descriptor; C5 a
    /// fresh source uses another provider and HTTPS target; C6 rotation supplies
    /// the exact frozen revision; C7 source is exactly one revision ahead with
    /// byte-identical material (the Vault commit/Session-CAS crash window); C8
    /// the one-ahead material differs; C9 the source has advanced by more than
    /// one revision.
    /// E1 one described source is published; E2 target drift conflicts before a
    /// secret write; E3 Workspace/target drift cannot rotate; E4 legacy rows are
    /// rejected rather than used as a candidate fallback; E5 the same generic
    /// authority admits another provider without a parallel service; E6 exact
    /// rotation publishes the successor revision; E7 an exact crash replay
    /// returns the already-completed revision without another write; E8
    /// different material cannot impersonate that replay; E9 a larger revision
    /// gap is never guessed or adopted.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | Effect |
    /// |---|---|---|---|---|---|---|---|---|---|---|
    /// | V1 | T | - | - | - | F | - | F | F | F | E1 |
    /// | V2 | F | F | - | T | F | - | F | F | F | E2 |
    /// | V3 | - | - | F | T | F | - | F | F | F | E3 |
    /// | V4 | - | - | - | F | F | - | F | F | F | E4 |
    /// | V5 | T | - | - | - | T | - | F | F | F | E5 |
    /// | V6 | - | - | T | T | F | T | F | F | F | E6 |
    /// | V7 | - | - | T | T | F | F | T | F | F | E7 |
    /// | V8 | - | - | T | T | F | F | F | T | F | E8 |
    /// | V9 | - | - | T | T | F | F | F | F | T | E9 |
    #[tokio::test]
    async fn material_ingress_is_consumer_neutral_and_rejects_binding_drift() {
        let secrets = std::sync::Arc::new(InMemorySecretStore::new());
        let repository = std::sync::Arc::new(InMemoryCredentialRepo::new());
        let vault = VaultState::new(secrets.clone(), repository.clone());
        let source_id = CredentialSourceId("credential:repository:exact".into());
        let exact_target = awaken_session_contract::repository_transport_credential_target(
            "https://github.com/awaken/example.git",
        )
        .expect("V1 target");
        let other_target = awaken_session_contract::repository_transport_credential_target(
            "https://git.example.test/awaken/example.git",
        )
        .expect("V5 target");

        let other_provider_source =
            CredentialSourceId("credential:repository:other-provider".into());
        let empty_inventory = secrets.inventory().await.expect("V5 inventory");
        vault
            .enter_material(enter(
                other_provider_source.clone(),
                "workspace-a",
                other_target.clone(),
                "git-example",
                "other-token",
            ))
            .await
            .expect("V5/E5");
        assert_ne!(secrets.inventory().await.unwrap(), empty_inventory, "V5/E5");
        assert_eq!(
            repository
                .get(&other_provider_source)
                .await
                .unwrap()
                .descriptor
                .unwrap()
                .provider
                .0,
            "git-example",
            "V5/E5"
        );

        vault
            .enter_material(enter(
                source_id.clone(),
                "workspace-a",
                exact_target.clone(),
                "github",
                "token-1",
            ))
            .await
            .expect("V1/E1");
        let source = repository.get(&source_id).await.expect("V1 source");
        assert!(source.provider_id.is_none(), "V1/E1 one provider authority");
        let descriptor = source.descriptor.as_ref().expect("V1 descriptor");
        assert_eq!(descriptor.provider.0, "github", "V1/E1");
        assert!(
            descriptor
                .admit(
                    &exact_target,
                    &awaken_session_contract::repository_transport_credential_usage(),
                )
                .is_ok(),
            "V1/E1"
        );

        let before = secrets.inventory().await.expect("V2 inventory");
        assert!(
            vault
                .enter_material(enter(
                    source_id.clone(),
                    "workspace-a",
                    other_target.clone(),
                    "github",
                    "token-2",
                ))
                .await
                .is_err(),
            "V2/E2"
        );
        assert_eq!(secrets.inventory().await.unwrap(), before, "V2/E2");
        assert_eq!(
            repository.get(&source_id).await.unwrap().version,
            1,
            "V2/E2"
        );

        for (rule, workspace, target) in [
            ("V3-workspace", "workspace-b", exact_target.clone()),
            ("V3-target", "workspace-a", other_target),
        ] {
            assert!(
                vault
                    .rotate_material(rotate(
                        source_id.clone(),
                        1,
                        workspace,
                        target,
                        "github",
                        "token-3",
                    ))
                    .await
                    .is_err(),
                "{rule}/E3"
            );
            assert_eq!(secrets.inventory().await.unwrap(), before, "{rule}/E3");
            assert_eq!(
                repository.get(&source_id).await.unwrap().version,
                1,
                "{rule}/E3"
            );
        }

        let exact_revision = vault
            .rotate_material(rotate(
                source_id.clone(),
                1,
                "workspace-a",
                exact_target.clone(),
                "github",
                "token-4",
            ))
            .await
            .expect("V6/E6 exact rotation");
        assert_eq!(exact_revision, 2, "V6/E6 completed revision");
        assert_eq!(
            repository.get(&source_id).await.unwrap().version,
            2,
            "V6/E6"
        );
        let exact_rotation_inventory = secrets.inventory().await.unwrap();
        let replay_revision = vault
            .rotate_material(rotate(
                source_id.clone(),
                1,
                "workspace-a",
                exact_target.clone(),
                "github",
                "token-4",
            ))
            .await
            .expect("V7/E7 exact crash replay");
        assert_eq!(replay_revision, 2, "V7/E7 completed revision");
        assert_eq!(
            secrets.inventory().await.unwrap(),
            exact_rotation_inventory,
            "V7/E7 no material write"
        );
        assert_eq!(
            repository.get(&source_id).await.unwrap().version,
            2,
            "V7/E7 exact revision remains"
        );
        assert!(
            vault
                .rotate_material(rotate(
                    source_id.clone(),
                    1,
                    "workspace-a",
                    exact_target.clone(),
                    "github",
                    "must-not-write",
                ))
                .await
                .is_err(),
            "V8/E8"
        );
        assert_eq!(
            secrets.inventory().await.unwrap(),
            exact_rotation_inventory,
            "V8/E8 no material write"
        );
        assert_eq!(
            repository.get(&source_id).await.unwrap().version,
            2,
            "V8/E8 exact revision remains"
        );

        let third_revision = vault
            .rotate_material(rotate(
                source_id.clone(),
                2,
                "workspace-a",
                exact_target.clone(),
                "github",
                "token-5",
            ))
            .await
            .expect("V9 setup advances to revision 3");
        assert_eq!(third_revision, 3, "V9 setup");
        let drift_inventory = secrets.inventory().await.unwrap();
        assert!(
            vault
                .rotate_material(rotate(
                    source_id.clone(),
                    1,
                    "workspace-a",
                    exact_target.clone(),
                    "github",
                    "token-5",
                ))
                .await
                .is_err(),
            "V9/E9"
        );
        assert_eq!(
            secrets.inventory().await.unwrap(),
            drift_inventory,
            "V9/E9 no material write"
        );
        assert_eq!(
            repository.get(&source_id).await.unwrap().version,
            3,
            "V9/E9 revision remains"
        );

        let legacy = awaken_credential_vault::repo::enter_credential(
            DomainCredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("github_repository".into()),
                env_key: None,
                secret: Some(RedactedString::new("legacy-token")),
                oauth_command: None,
            },
            secrets.as_ref(),
            repository.as_ref(),
        )
        .await
        .expect("V4 legacy fixture");
        let legacy_before = secrets.inventory().await.unwrap();
        assert!(
            vault
                .rotate_material(rotate(
                    legacy.id.clone(),
                    1,
                    "workspace-a",
                    exact_target,
                    "github",
                    "must-not-write",
                ))
                .await
                .is_err(),
            "V4/E4"
        );
        assert_eq!(secrets.inventory().await.unwrap(), legacy_before, "V4/E4");
        assert_eq!(
            repository.get(&legacy.id).await.unwrap().version,
            1,
            "V4/E4"
        );
    }
}
