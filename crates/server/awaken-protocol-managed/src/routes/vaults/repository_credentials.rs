//! Repository credential ingress projection over the canonical Vault owner.
//!
//! This module owns no material or source state: it translates Repository token
//! ingress into the existing Credential WAL/CAS operations implemented by the
//! parent `VaultState`.

use std::collections::BTreeMap;

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    CredentialDescriptor, CredentialMaterialDescriptor, CredentialSourceId, CredentialTarget,
    CredentialTargetContract, HTTP_BASIC_MATERIAL_TYPE, http_basic_material,
};
use awaken_credential_vault::repo::{CredentialMaterialPatch, rotate_credential_materials_exact};
use awaken_credential_vault::{
    CredentialCreateParams as DomainCredentialCreateParams, CredentialKind, CredentialStatus,
};
use awaken_session_application::RepositoryCredentialIngress;

use super::VaultState;

#[async_trait::async_trait]
impl RepositoryCredentialIngress for VaultState {
    async fn enter_repository_token(
        &self,
        source_id: CredentialSourceId,
        workspace_id: &str,
        target: CredentialTarget,
        token: RedactedString,
    ) -> Result<CredentialSourceId, String> {
        validate_github_repository_target(&target)?;
        let material = repository_http_basic_material(token).map_err(|error| error.to_string())?;
        let descriptor = CredentialDescriptor::new(
            "github",
            CredentialMaterialDescriptor::structured(
                HTTP_BASIC_MATERIAL_TYPE,
                ["password", "username"],
            ),
            [CredentialTargetContract::new(
                target,
                awaken_session_contract::repository_transport_credential_usage(),
            )],
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
        Ok(entry.source.id)
    }

    async fn rotate_repository_token(
        &self,
        source_id: &CredentialSourceId,
        expected_revision: u64,
        workspace_id: &str,
        target: CredentialTarget,
        token: RedactedString,
    ) -> Result<u64, String> {
        validate_github_repository_target(&target)?;
        let current = self
            .repository
            .get(source_id)
            .await
            .map_err(|error| error.to_string())?;
        let repository_usage = awaken_session_contract::repository_transport_credential_usage();
        let canonical_provider = current.descriptor.as_ref().is_some_and(|descriptor| {
            descriptor.provider.0 == "github"
                && descriptor.admit(&target, &repository_usage).is_ok()
        });
        if current.workspace_id != workspace_id || !canonical_provider {
            return Err("repository credential binding is unavailable in this Workspace".into());
        }
        let material = repository_http_basic_material(token).map_err(|error| error.to_string())?;
        let expected_version = i64::try_from(expected_revision)
            .map_err(|_| "repository credential revision exceeds the Vault range".to_string())?;
        let completed_version = expected_version
            .checked_add(1)
            .ok_or_else(|| "repository credential revision exceeds the Vault range".to_string())?;
        if current.version == completed_version {
            if current.status != CredentialStatus::Active {
                return Err("repository credential is not active".into());
            }
            let sealed = awaken_credential_vault::materialize(&current, self.secrets.as_ref())
                .await
                .map_err(|error| error.to_string())?;
            if sealed.expose_secret() != material.expose_secret() {
                return Err(
                    "repository credential revision changed before material rotation".into(),
                );
            }
            return u64::try_from(current.version)
                .map_err(|_| "repository credential revision exceeds the Session range".into());
        }
        if current.version != expected_version {
            return Err("repository credential revision changed before material rotation".into());
        }
        let rotated = rotate_credential_materials_exact(
            source_id,
            expected_version,
            CredentialMaterialPatch {
                primary: Some(material),
                auxiliary: BTreeMap::new(),
                descriptor: None,
            },
            self.secrets.as_ref(),
            self.repository.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        u64::try_from(rotated.version)
            .map_err(|_| "repository credential revision exceeds the Session range".into())
    }
}

fn validate_github_repository_target(target: &CredentialTarget) -> Result<(), String> {
    let expected =
        awaken_session_contract::repository_transport_credential_target("https://github.com")
            .map_err(|error| error.to_string())?;
    if target != &expected {
        return Err(
            "GitHub repository credentials require the canonical https://github.com/git target"
                .into(),
        );
    }
    Ok(())
}

pub(super) fn repository_http_basic_material(
    token: RedactedString,
) -> Result<RedactedString, awaken_credential_vault::CredentialError> {
    awaken_credential_vault::encode_structured_material(http_basic_material(
        RedactedString::new("x-access-token"),
        token,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
    use awaken_credential_vault::{InMemorySecretStore, SecretStore};

    /// Repository ingress cause/effect graph: C1 Workspace and typed target are
    /// exact and its canonical audience is GitHub; C2 an idempotent replay keeps
    /// the same target; C3 rotation keeps Workspace and target exact; C4 a
    /// legacy undescribed row is supplied; C5 a fresh ingress carries another
    /// HTTPS host; C6 rotation supplies the exact frozen revision; C7 the source
    /// is exactly one revision ahead with byte-identical material (the Vault
    /// commit/Session-CAS crash window); C8 the one-ahead material differs; C9
    /// the source has advanced by more than one revision.
    /// E1 one described source is published; E2 target drift conflicts before a
    /// secret write; E3 Workspace/target drift cannot rotate; E4 legacy rows are
    /// rejected rather than used as a candidate fallback; E5 wrong-host ingress
    /// publishes neither a source nor secret; E6 exact rotation publishes the
    /// successor revision; E7 an exact crash replay returns the already-completed
    /// revision without another write; E8 different material cannot impersonate
    /// that replay; E9 a larger revision gap is never guessed or adopted.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | Effect |
    /// |---|---|---|---|---|---|---|---|---|---|---|
    /// | V1 | T | T | T | F | F | - | F | F | F | E1 |
    /// | V2 | T | F | - | F | F | - | F | F | F | E2 |
    /// | V3 | T | T | F | F | F | - | F | F | F | E3 |
    /// | V4 | - | - | - | T | F | - | F | F | F | E4 |
    /// | V5 | F | - | - | F | T | - | F | F | F | E5 |
    /// | V6 | T | T | T | F | F | T | F | F | F | E6 |
    /// | V7 | T | T | T | F | F | F | T | F | F | E7 |
    /// | V8 | T | T | T | F | F | F | F | T | F | E8 |
    /// | V9 | T | T | T | F | F | F | F | F | T | E9 |
    #[tokio::test]
    async fn repository_ingress_publishes_one_described_target_and_rejects_drift() {
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

        let wrong_host_source = CredentialSourceId("credential:repository:wrong-host".into());
        let empty_inventory = secrets.inventory().await.expect("V5 inventory");
        assert!(
            vault
                .enter_repository_token(
                    wrong_host_source.clone(),
                    "workspace-a",
                    other_target.clone(),
                    RedactedString::new("must-not-write"),
                )
                .await
                .is_err(),
            "V5/E5"
        );
        assert_eq!(
            secrets.inventory().await.unwrap(),
            empty_inventory,
            "V5/E5 no secret"
        );
        assert!(
            repository.get(&wrong_host_source).await.is_err(),
            "V5/E5 no source"
        );

        vault
            .enter_repository_token(
                source_id.clone(),
                "workspace-a",
                exact_target.clone(),
                RedactedString::new("token-1"),
            )
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
                .enter_repository_token(
                    source_id.clone(),
                    "workspace-a",
                    other_target.clone(),
                    RedactedString::new("token-2"),
                )
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
                    .rotate_repository_token(
                        &source_id,
                        1,
                        workspace,
                        target,
                        RedactedString::new("token-3"),
                    )
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
            .rotate_repository_token(
                &source_id,
                1,
                "workspace-a",
                exact_target.clone(),
                RedactedString::new("token-4"),
            )
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
            .rotate_repository_token(
                &source_id,
                1,
                "workspace-a",
                exact_target.clone(),
                RedactedString::new("token-4"),
            )
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
                .rotate_repository_token(
                    &source_id,
                    1,
                    "workspace-a",
                    exact_target.clone(),
                    RedactedString::new("must-not-write"),
                )
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
            .rotate_repository_token(
                &source_id,
                2,
                "workspace-a",
                exact_target.clone(),
                RedactedString::new("token-5"),
            )
            .await
            .expect("V9 setup advances to revision 3");
        assert_eq!(third_revision, 3, "V9 setup");
        let drift_inventory = secrets.inventory().await.unwrap();
        assert!(
            vault
                .rotate_repository_token(
                    &source_id,
                    1,
                    "workspace-a",
                    exact_target.clone(),
                    RedactedString::new("token-5"),
                )
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
                .rotate_repository_token(
                    &legacy.id,
                    1,
                    "workspace-a",
                    exact_target,
                    RedactedString::new("must-not-write"),
                )
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
