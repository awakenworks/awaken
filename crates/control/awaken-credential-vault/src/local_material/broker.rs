//! Local Vault arm of the existing Sandbox `SecretBroker` port.

use std::collections::BTreeMap;

use awaken_agent_contract::RedactedString;
use awaken_provisioning_contract::{
    SandboxError, SecretBroker, SecretWritebackEffect, SecretWritebackTarget,
};

use super::{
    CredentialMaterialError, CredentialMaterialPatch, CredentialSource,
    VaultLocalMaterialAuthority, rotate_credential_materials_exact,
    write_back_credential_material_for_effect,
};
use crate::{CredentialError, CredentialSourceId};

fn split_exact_vault_reference(reference: &str) -> Result<(&str, Option<u64>), String> {
    if reference.is_empty() {
        return Err("credential reference is empty".into());
    }
    let Some((source_id, revision)) = reference.rsplit_once('@') else {
        return Ok((reference, None));
    };
    if revision.is_empty() || !revision.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok((reference, None));
    }
    let revision = revision
        .parse::<u64>()
        .map_err(|_| "credential reference revision is invalid".to_string())?;
    if source_id.is_empty() || revision == 0 {
        return Err("credential reference must name a source and positive revision".into());
    }
    Ok((source_id, Some(revision)))
}

/// Exact positive revision kernel shared by targetless reads and write-back.
/// Kept with the Vault authority so a neutral materializer cannot approve a
/// stale source by reasoning about a raw string reference independently.
fn exact_vault_revision_is_admitted(actual: i64, expected: Option<u64>) -> bool {
    actual > 0 && expected.is_none_or(|expected| u64::try_from(actual) == Ok(expected))
}

#[cfg(kani)]
mod kani_proofs {
    use super::exact_vault_revision_is_admitted;

    /// @obligation credential_reference.exact_revision_never_resolves_another_revision
    #[kani::proof]
    fn exact_vault_revision_accepts_only_a_positive_matching_or_unpinned_source() {
        let actual: i64 = kani::any();
        let has_pin: bool = kani::any();
        let expected: u64 = kani::any();
        let requested = has_pin.then_some(expected);
        assert_eq!(
            exact_vault_revision_is_admitted(actual, requested),
            actual > 0 && (!has_pin || u64::try_from(actual) == Ok(expected))
        );
    }
}

fn verify_exact_vault_revision(
    source: &CredentialSource,
    revision: Option<u64>,
) -> Result<(), String> {
    if source.version <= 0 {
        return Err("credential material revision is invalid".into());
    }
    if !exact_vault_revision_is_admitted(source.version, revision) {
        return Err("credential material revision mismatch".into());
    }
    Ok(())
}

struct CredentialWritebackRequest {
    source_id: String,
    revision: Option<u64>,
    material: RedactedString,
}

fn credential_writeback_request(
    reference: &str,
    bytes: Vec<u8>,
) -> Result<CredentialWritebackRequest, SandboxError> {
    let (source_id, revision) =
        split_exact_vault_reference(reference).map_err(SandboxError::invalid)?;
    let material = String::from_utf8(bytes)
        .map_err(|_| SandboxError::invalid("credential write-back is not valid UTF-8"))?;
    Ok(CredentialWritebackRequest {
        source_id: source_id.to_owned(),
        revision,
        material: RedactedString::new(material),
    })
}

#[async_trait::async_trait]
impl SecretBroker for VaultLocalMaterialAuthority {
    async fn materialize(&self, reference: &str) -> Result<Vec<u8>, SandboxError> {
        let (source_id, revision) =
            split_exact_vault_reference(reference).map_err(SandboxError::invalid)?;
        let source = self
            .load_active_source(source_id)
            .await
            .map_err(|error| credential_material_error("materialize", error))?;
        verify_exact_vault_revision(&source, revision).map_err(SandboxError::conflict)?;
        if source.descriptor.is_some() {
            return Err(SandboxError::invalid(
                "described credentials require an exact target and cannot use a targetless secret mount",
            ));
        }
        self.materialize_source(&source)
            .await
            .map(|secret| secret.expose_secret().as_bytes().to_vec())
            .map_err(|error| credential_material_error("materialize", error))
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, SandboxError> {
        Err(SandboxError::unsupported(
            "Vault material is not a claim-fenced process-secret reference",
        ))
    }

    async fn write_back(
        &self,
        target: SecretWritebackTarget<'_>,
        bytes: Vec<u8>,
    ) -> Result<(), SandboxError> {
        match target {
            SecretWritebackTarget::Unattached { reference } => {
                self.write_back_unattached(reference, bytes).await
            }
            SecretWritebackTarget::Effect(effect) => self.write_back_effect(effect, bytes).await,
        }
    }
}

impl VaultLocalMaterialAuthority {
    async fn write_back_unattached(
        &self,
        reference: &str,
        bytes: Vec<u8>,
    ) -> Result<(), SandboxError> {
        let request = credential_writeback_request(reference, bytes)?;
        let source = self
            .load_active_source(&request.source_id)
            .await
            .map_err(|error| credential_material_error("write_back", error))?;
        verify_exact_vault_revision(&source, request.revision).map_err(SandboxError::conflict)?;
        if source.descriptor.is_some() {
            return Err(SandboxError::invalid(
                "described credentials require an exact target and cannot use targetless write-back",
            ));
        }
        source.material_ref.as_ref().ok_or_else(|| {
            SandboxError::not_found(format!("credential {} has no material", source.id.0))
        })?;
        rotate_credential_materials_exact(
            &source.id,
            source.version,
            CredentialMaterialPatch {
                primary: Some(request.material),
                auxiliary: BTreeMap::new(),
                descriptor: None,
            },
            self.secrets.as_ref(),
            self.credentials.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map(|_| ())
        .map_err(|error| credential_vault_error("write_back", error))
    }

    async fn write_back_effect(
        &self,
        effect: &SecretWritebackEffect,
        bytes: Vec<u8>,
    ) -> Result<(), SandboxError> {
        effect
            .authorization()
            .validate_live_at(awaken_time::WallClockExt::now_ms(self.clock.as_ref()))?;
        let request = credential_writeback_request(effect.reference(), bytes)?;
        let revision = request.revision.ok_or_else(|| {
            SandboxError::invalid(
                "effect-fenced credential write-back requires an exact source revision",
            )
        })?;
        let expected_version = i64::try_from(revision).map_err(|_| {
            SandboxError::invalid("credential reference revision exceeds the authority range")
        })?;
        write_back_credential_material_for_effect(
            &CredentialSourceId(request.source_id),
            expected_version,
            effect.writeback_id(),
            request.material,
            self.secrets.as_ref(),
            self.credentials.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map(|_| ())
        .map_err(|error| credential_vault_error("write_back_for_effect", error))
    }
}

fn credential_material_error(
    operation: &'static str,
    error: CredentialMaterialError,
) -> SandboxError {
    use CredentialMaterialError as E;
    let detail = error.to_string();
    match error {
        E::RevisionMismatch
        | E::RecipientMismatch
        | E::BindingMismatch
        | E::PayloadMismatch
        | E::ResolverMismatch => SandboxError::Conflict(detail),
        E::EnvelopeExpired | E::Expired => SandboxError::Gone(detail),
        E::MaterialKindMismatch => SandboxError::Unsupported(detail),
        E::LoginRequired | E::Invalid | E::Disabled => SandboxError::Unauthorized(detail),
        E::Unavailable | E::ProbeFailed => SandboxError::backend(operation, detail),
    }
}

fn credential_vault_error(operation: &'static str, error: CredentialError) -> SandboxError {
    use CredentialError as E;
    let detail = error.to_string();
    match error {
        E::SecretNotFound(_)
        | E::SourceNotFound(_)
        | E::PoolNotFound(_)
        | E::NoCredential
        | E::MissingMaterialRef(_) => SandboxError::NotFound(detail),
        E::NotActive(_) | E::OAuth(_) => SandboxError::Unauthorized(detail),
        E::EnvironmentSourceUnsupported(_) | E::WorkerLocalSourceUnsupported(_) => {
            SandboxError::Unsupported(detail)
        }
        E::InvalidSource(_) => SandboxError::Invalid(detail),
        E::MutationConflict(_) => SandboxError::Conflict(detail),
        E::Seal | E::Storage(_) => SandboxError::backend(operation, detail),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use crate::repo::{
        CredentialRepo, InMemoryCredentialRepo, enter_credential,
        enter_credential_idempotent_described,
    };
    use crate::{CredentialCreateParams, CredentialKind, InMemorySecretStore, SecretStore};
    use awaken_agent_contract::RedactedString;
    use awaken_agent_contract::{
        credential::CredentialPurpose, credential::CredentialTarget, credential::CredentialUsage,
        credential::HttpEffectPlacement,
    };
    use awaken_credential_contract::{
        CredentialDescriptor, CredentialMaterialDescriptor, CredentialSourceId,
        CredentialTargetContract, OPAQUE_SECRET_MATERIAL_TYPE,
    };
    use awaken_provisioning_contract::{SecretBroker, SecretWritebackEffect};

    use super::VaultLocalMaterialAuthority;

    /// Targetless SecretBroker cause/effect graph: C1 a legacy undescribed
    /// source names an exact revision; C2 the requested revision drifts; C3 a
    /// described HttpEffect source carries target/usage authority which this
    /// mount wire cannot express. Effects: E1 exact legacy compatibility read;
    /// E2 stale revision fails; E3 described read/write-back fail before
    /// plaintext or mutation.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | M1 | T | F | F | E1 |
    /// | M2 | F | T | F | E2 |
    /// | M3 | - | - | T | E3 |
    #[tokio::test]
    async fn targetless_secret_broker_accepts_only_exact_legacy_sources() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let legacy = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("github.com".into()),
                env_key: None,
                secret: Some(RedactedString::new("repository-token")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("M1 legacy source");

        let usage = CredentialUsage::HttpEffect {
            fields: BTreeMap::from([(
                "token".into(),
                BTreeSet::from([HttpEffectPlacement::Header {
                    name: "authorization".into(),
                }]),
            )]),
        };
        let descriptor = CredentialDescriptor::new(
            "github",
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
            [CredentialTargetContract::new(
                CredentialTarget::new(
                    CredentialPurpose::HttpEffect,
                    "awaken.test/github-http-effect",
                ),
                usage,
            )],
        );
        let described = enter_credential_idempotent_described(
            CredentialSourceId("credential:described-http-effect".into()),
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("must-not-open")),
                oauth_command: None,
            },
            None,
            descriptor,
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("M3 described source")
        .source;
        let broker = VaultLocalMaterialAuthority::new(
            credentials.clone(),
            secrets.clone(),
            awaken_time::system_clock(),
        );

        let legacy_reference = format!("{}@{}", legacy.id.0, legacy.version);
        assert_eq!(
            broker.materialize(&legacy_reference).await.unwrap(),
            b"repository-token",
            "M1/E1"
        );
        assert!(
            broker
                .materialize(&format!("{}@2", legacy.id.0))
                .await
                .is_err(),
            "M2/E2"
        );

        let described_reference = format!("{}@{}", described.id.0, described.version);
        let inventory = secrets.inventory().await.expect("M3 inventory");
        assert!(
            broker.materialize(&described_reference).await.is_err(),
            "M3/E3 read"
        );
        assert!(
            broker
                .write_back(
                    awaken_provisioning_contract::SecretWritebackTarget::Unattached {
                        reference: &described_reference
                    },
                    b"must-not-write".to_vec()
                )
                .await
                .is_err(),
            "M3/E3 write-back"
        );
        assert_eq!(secrets.inventory().await.unwrap(), inventory, "M3/E3");
        assert_eq!(
            credentials.get(&described.id).await.unwrap().version,
            described.version,
            "M3/E3"
        );
    }

    /// Legacy write-back cause/effect graph: C1 the targetless source is active
    /// at the requested revision; C2 that exact revision was already replaced.
    /// Effects: E1 one exact successor is published; E2 stale replay changes no
    /// metadata or material.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | W1 | T | F | E1 |
    /// | W2 | F | T | E2 |
    #[tokio::test]
    async fn legacy_write_back_uses_the_exact_vault_revision() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("tool".into()),
                env_key: None,
                secret: Some(RedactedString::new("before")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("W1 source");
        let reference = format!("{}@{}", source.id.0, source.version);
        let broker = VaultLocalMaterialAuthority::new(
            credentials.clone(),
            secrets.clone(),
            awaken_time::system_clock(),
        );

        assert_eq!(broker.materialize(&reference).await.unwrap(), b"before");
        assert!(
            broker.materialize_process(&source.id.0).await.is_err(),
            "durable file ids are never process capabilities"
        );
        broker
            .write_back(
                awaken_provisioning_contract::SecretWritebackTarget::Unattached {
                    reference: &reference,
                },
                b"after".to_vec(),
            )
            .await
            .expect("W1/E1");
        assert_eq!(
            credentials.get(&source.id).await.unwrap().version,
            2,
            "W1/E1"
        );
        assert_eq!(broker.materialize(&source.id.0).await.unwrap(), b"after");

        let inventory = secrets.inventory().await.expect("W2 inventory");
        assert!(
            broker
                .write_back(
                    awaken_provisioning_contract::SecretWritebackTarget::Unattached {
                        reference: &reference
                    },
                    b"stale-write".to_vec()
                )
                .await
                .is_err(),
            "W2/E2"
        );
        assert_eq!(secrets.inventory().await.unwrap(), inventory, "W2/E2");
        assert_eq!(
            credentials.get(&source.id).await.unwrap().version,
            2,
            "W2/E2"
        );
    }

    /// Effect-aware write-back decision table. C1 the durable reference pins
    /// the exact source revision; C2 the aggregate authorization fence is live;
    /// C3 the physical Sandbox incarnation is same/different; C4 bytes are
    /// same/different; C5 the aggregate operation is predecessor/successor.
    /// Effects: E1 publishes one successor; E2 the same reference+physical
    /// identity and bytes replays across C5 without another rotation; E3 an
    /// unpinned reference, expired authorization, different physical identity,
    /// or different bytes fails without another credential/material mutation.
    ///
    /// | Rule | C1 pinned | C2 live | C3 physical | C4 bytes | C5 operation | Effect |
    /// |---|---|---|---|---|---|---|
    /// | F1 | T | T | exact | exact | predecessor | E1 |
    /// | F2 | T | T | exact | exact | successor | E2 |
    /// | F3 | F | T | exact | exact | any | E3 |
    /// | F4 | T | F | exact | exact | any | E3 |
    /// | F5 | T | T | different | exact | successor | E3 |
    /// | F6 | T | T | exact | different | any | E3 |
    #[tokio::test]
    async fn effect_write_back_is_pinned_and_idempotent_at_the_broker_boundary() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("native-cli".into()),
                env_key: None,
                secret: Some(RedactedString::new("before")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("F1 source");
        let reference = format!("{}@{}", source.id.0, source.version);
        let broker = VaultLocalMaterialAuthority::new(
            credentials.clone(),
            secrets.clone(),
            awaken_time::system_clock(),
        );
        let predecessor = awaken_provisioning_contract::SandboxEffectFence::new(
            "continuation-writeback",
            "worker-owner",
            "worker-incarnation",
            7,
            u64::MAX,
        )
        .unwrap();
        let successor = awaken_provisioning_contract::SandboxEffectFence::new(
            "terminal-writeback",
            "worker-owner",
            "worker-incarnation",
            7,
            u64::MAX,
        )
        .unwrap();
        let first_effect =
            SecretWritebackEffect::new(reference.clone(), "pod-uid-a", predecessor).unwrap();
        let successor_effect =
            SecretWritebackEffect::new(reference.clone(), "pod-uid-a", successor.clone()).unwrap();

        broker
            .write_back(
                awaken_provisioning_contract::SecretWritebackTarget::Effect(&first_effect),
                b"after".to_vec(),
            )
            .await
            .expect("F1/E1");
        broker
            .write_back(
                awaken_provisioning_contract::SecretWritebackTarget::Effect(&successor_effect),
                b"after".to_vec(),
            )
            .await
            .expect("F2/E2 cross-operation replay");
        let published = credentials.get(&source.id).await.unwrap();
        assert_eq!(published.version, source.version + 1, "F1/F2");
        assert_eq!(secrets.inventory().await.unwrap().len(), 1, "F1/F2");

        let unpinned =
            SecretWritebackEffect::new(source.id.0.clone(), "pod-uid-a", successor.clone())
                .unwrap();
        assert!(
            broker
                .write_back(
                    awaken_provisioning_contract::SecretWritebackTarget::Effect(&unpinned),
                    b"after".to_vec()
                )
                .await
                .is_err(),
            "F3/E3"
        );
        let expired = awaken_provisioning_contract::SandboxEffectFence::new(
            "terminal-writeback-1",
            "worker-owner",
            "worker-incarnation",
            7,
            0,
        )
        .unwrap();
        let expired_effect =
            SecretWritebackEffect::new(reference.clone(), "pod-uid-a", expired).unwrap();
        assert!(
            broker
                .write_back(
                    awaken_provisioning_contract::SecretWritebackTarget::Effect(&expired_effect),
                    b"after".to_vec()
                )
                .await
                .is_err(),
            "F4/E3"
        );
        let different_physical =
            SecretWritebackEffect::new(reference, "pod-uid-b", successor).unwrap();
        assert!(
            broker
                .write_back(
                    awaken_provisioning_contract::SecretWritebackTarget::Effect(
                        &different_physical
                    ),
                    b"after".to_vec()
                )
                .await
                .is_err(),
            "F5/E3 physical incarnation"
        );
        assert!(
            broker
                .write_back(
                    awaken_provisioning_contract::SecretWritebackTarget::Effect(&successor_effect),
                    b"different".to_vec()
                )
                .await
                .is_err(),
            "F6/E3 material"
        );
        assert_eq!(
            credentials.get(&source.id).await.unwrap(),
            published,
            "F3-F6"
        );
        assert_eq!(secrets.inventory().await.unwrap().len(), 1, "F3-F6");
    }
}

#[cfg(test)]
mod exact_revision_tests {
    use super::exact_vault_revision_is_admitted;

    #[test]
    fn invalid_or_mismatched_source_revisions_fail_closed() {
        assert!(!exact_vault_revision_is_admitted(-1, None));
        assert!(!exact_vault_revision_is_admitted(0, None));
        assert!(exact_vault_revision_is_admitted(7, None));
        assert!(exact_vault_revision_is_admitted(7, Some(7)));
        assert!(!exact_vault_revision_is_admitted(7, Some(8)));
    }
}
