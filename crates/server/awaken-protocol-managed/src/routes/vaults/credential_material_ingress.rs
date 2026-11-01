// @formal surface exclude=test_support_adapter
// @formal owner crates/control/awaken-credential-vault/src/vault_credential_access.rs
// @formal reason: The original consumer-neutral ingress regression remains at its historical
// @formal reason: module path; all production commands moved to the credential access owner.
//! Consumer-neutral ingress regression over the canonical Vault access owner.

use super::VaultState;
use awaken_credential_contract::{
    CredentialMaterialIngressCommand, CredentialMaterialInput, CredentialMaterialRotationCommand,
};

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_contract::CredentialSourceId;
    use awaken_credential_contract::http_basic_material;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
    use awaken_credential_vault::{
        CredentialCreateParams as DomainCredentialCreateParams, CredentialKind,
    };
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
        target: awaken_agent_contract::credential::CredentialTarget,
        provider: &str,
        token: &str,
    ) -> CredentialMaterialIngressCommand {
        CredentialMaterialIngressCommand {
            source_id,
            workspace_id: workspace_id.into(),
            target,
            usage: awaken_session_configuration_contract::resource::repository_transport_credential_usage(),
            material: material(provider, token),
        }
    }

    fn rotate(
        source_id: CredentialSourceId,
        expected_revision: u64,
        workspace_id: &str,
        target: awaken_agent_contract::credential::CredentialTarget,
        provider: &str,
        token: &str,
    ) -> CredentialMaterialRotationCommand {
        CredentialMaterialRotationCommand {
            source_id,
            expected_revision,
            workspace_id: workspace_id.into(),
            target,
            usage: awaken_session_configuration_contract::resource::repository_transport_credential_usage(),
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
        let vault = VaultState::new(
            secrets.clone(),
            repository.clone(),
            awaken_time::system_clock(),
            None,
        );
        let source_id = CredentialSourceId("credential:repository:exact".into());
        let exact_target =
            awaken_session_configuration_contract::resource::repository_transport_credential_target(
                "https://github.com/awaken/example.git",
            )
            .expect("V1 target");
        let other_target =
            awaken_session_configuration_contract::resource::repository_transport_credential_target(
                "https://git.example.test/awaken/example.git",
            )
            .expect("V5 target");

        let other_provider_source =
            CredentialSourceId("credential:repository:other-provider".into());
        let empty_inventory = secrets.inventory().await.expect("V5 inventory");
        awaken_credential_contract::CredentialMaterialIngress::enter_material(
            vault.application().credential_access().as_ref(),
            enter(
                other_provider_source.clone(),
                "workspace-a",
                other_target.clone(),
                "git-example",
                "other-token",
            ),
        )
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

        // Material-entry causes/effects: M1 absent exact source => the original
        // WAL publishes one source and returns created=true; M2 an exact retry
        // => the same credential with created=false, no source or material change.
        // This is the original insertion winner, not Session/Registry provenance.
        let entered = awaken_credential_contract::CredentialMaterialIngress::enter_material(
            vault.application().credential_access().as_ref(),
            enter(
                source_id.clone(),
                "workspace-a",
                exact_target.clone(),
                "github",
                "token-1",
            ),
        )
        .await
        .expect("V1/E1");
        assert!(entered.created, "M1 current command owns the new material");
        let source = repository.get(&source_id).await.expect("V1 source");
        assert!(source.provider_id.is_none(), "V1/E1 one provider authority");
        let descriptor = source.descriptor.as_ref().expect("V1 descriptor");
        assert_eq!(descriptor.provider.0, "github", "V1/E1");
        assert!(
            descriptor
                .admit(
                    &exact_target,
                    &awaken_session_configuration_contract::resource::repository_transport_credential_usage(),
                )
                .is_ok(),
            "V1/E1"
        );

        let before = secrets.inventory().await.expect("V2 inventory");
        let replayed = awaken_credential_contract::CredentialMaterialIngress::enter_material(
            vault.application().credential_access().as_ref(),
            enter(
                source_id.clone(),
                "workspace-a",
                exact_target.clone(),
                "github",
                "token-1",
            ),
        )
        .await
        .expect("M2 exact replay");
        assert!(
            !replayed.created,
            "M2 earlier durable truth owns the material"
        );
        assert_eq!(replayed.credential, entered.credential, "M2 exact identity");
        assert_eq!(
            repository.get(&source_id).await.unwrap(),
            source,
            "M2 source"
        );
        assert_eq!(secrets.inventory().await.unwrap(), before, "M2 material");
        assert!(
            awaken_credential_contract::CredentialMaterialIngress::enter_material(
                vault.application().credential_access().as_ref(),
                enter(
                    source_id.clone(),
                    "workspace-a",
                    other_target.clone(),
                    "github",
                    "token-2",
                )
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
                awaken_credential_contract::CredentialMaterialIngress::rotate_material(
                    vault.application().credential_access().as_ref(),
                    rotate(source_id.clone(), 1, workspace, target, "github", "token-3",)
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

        let exact_revision =
            awaken_credential_contract::CredentialMaterialIngress::rotate_material(
                vault.application().credential_access().as_ref(),
                rotate(
                    source_id.clone(),
                    1,
                    "workspace-a",
                    exact_target.clone(),
                    "github",
                    "token-4",
                ),
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
        let replay_revision =
            awaken_credential_contract::CredentialMaterialIngress::rotate_material(
                vault.application().credential_access().as_ref(),
                rotate(
                    source_id.clone(),
                    1,
                    "workspace-a",
                    exact_target.clone(),
                    "github",
                    "token-4",
                ),
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
            awaken_credential_contract::CredentialMaterialIngress::rotate_material(
                vault.application().credential_access().as_ref(),
                rotate(
                    source_id.clone(),
                    1,
                    "workspace-a",
                    exact_target.clone(),
                    "github",
                    "must-not-write",
                )
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

        let third_revision =
            awaken_credential_contract::CredentialMaterialIngress::rotate_material(
                vault.application().credential_access().as_ref(),
                rotate(
                    source_id.clone(),
                    2,
                    "workspace-a",
                    exact_target.clone(),
                    "github",
                    "token-5",
                ),
            )
            .await
            .expect("V9 setup advances to revision 3");
        assert_eq!(third_revision, 3, "V9 setup");
        let drift_inventory = secrets.inventory().await.unwrap();
        assert!(
            awaken_credential_contract::CredentialMaterialIngress::rotate_material(
                vault.application().credential_access().as_ref(),
                rotate(
                    source_id.clone(),
                    1,
                    "workspace-a",
                    exact_target.clone(),
                    "github",
                    "token-5",
                )
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
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("V4 legacy fixture");
        let legacy_before = secrets.inventory().await.unwrap();
        assert!(
            awaken_credential_contract::CredentialMaterialIngress::rotate_material(
                vault.application().credential_access().as_ref(),
                rotate(
                    legacy.id.clone(),
                    1,
                    "workspace-a",
                    exact_target,
                    "github",
                    "must-not-write",
                )
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
