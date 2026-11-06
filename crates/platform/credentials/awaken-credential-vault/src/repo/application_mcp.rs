// @formal surface obligation=credential.visible_row_always_has_material
// @formal surface obligation=credential.failed_create_compensates_material
// @formal surface obligation=credential.rotation_revision_cas
// @formal reason: MCP credential aggregate repository path.
//! Hosted application MCP bearer commands over the credential aggregate.

use std::collections::BTreeMap;

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::{
    credential::CredentialPurpose, credential::CredentialTarget, credential::CredentialUsage,
};
use awaken_credential_contract::{
    CredentialDescriptor, CredentialMaterialDescriptor, CredentialSourceId,
    CredentialTargetContract, OPAQUE_SECRET_MATERIAL_TYPE,
};
use awaken_time::WallClock;

use super::{
    CredentialMaterialPatch, CredentialRepo, LegacyProviderDescriptorMigration,
    enter_prepared_credential_idempotent, migrate_legacy_provider_to_descriptor_exact,
    rotate_credential_materials_exact_with_primary_ref,
};
use crate::{CredentialError, CredentialKind, CredentialSource, CredentialStatus, SecretStore};

/// Provider marker for one hosted application's static MCP bearer.
pub const APPLICATION_MCP_PROVIDER_ID: &str = "awaken.application-mcp.static-bearer/v1";

/// Write-only application command; its bearer is never serialized.
pub struct ApplicationMcpBearerCommand {
    pub source_id: CredentialSourceId,
    pub workspace_id: String,
    pub target_fingerprint: String,
    /// Canonical MCP HTTP URL selected by the target-owning application.
    pub target_audience: String,
    pub command_key_fingerprint: String,
    /// Caller-owned positive monotonic order; never an Awaken-side counter.
    pub credential_generation: u64,
    pub bearer: RedactedString,
}

pub struct PreparedApplicationMcpBearerRotation {
    pub after_source: CredentialSource,
    pub material_ref: crate::SecretRef,
    pub bearer: RedactedString,
    pub operation_id: String,
}

/// Validate the coordinates shared by the deterministic material and outbox
/// identities.
fn validate_identity_coordinates(
    source_id: &CredentialSourceId,
    command_key_fingerprint: &str,
    credential_generation: u64,
) -> Result<(), CredentialError> {
    if source_id.0.trim().is_empty()
        || command_key_fingerprint.trim().is_empty()
        || credential_generation == 0
    {
        return Err(CredentialError::InvalidSource(
            "application MCP credential source, idempotency and positive generation are required"
                .into(),
        ));
    }
    Ok(())
}

/// Build the one deterministic material identity used by both plain and
/// Managed application-MCP credential admission.
pub fn application_mcp_material_ref(
    source_id: &CredentialSourceId,
    command_key_fingerprint: &str,
    credential_generation: u64,
) -> Result<crate::SecretRef, CredentialError> {
    validate_identity_coordinates(source_id, command_key_fingerprint, credential_generation)?;
    Ok(crate::SecretRef(format!(
        "sec:{}:application-mcp:{}:{command_key_fingerprint}:generation:{credential_generation}",
        source_id.0,
        command_key_fingerprint.len(),
    )))
}

/// Derive the existing Managed outbox identity for one prepared rotation.
pub fn application_mcp_operation_id(
    source_id: &CredentialSourceId,
    prior_source_version: i64,
    command_key_fingerprint: &str,
    credential_generation: u64,
) -> Result<String, CredentialError> {
    validate_identity_coordinates(source_id, command_key_fingerprint, credential_generation)?;
    if prior_source_version <= 0 {
        return Err(CredentialError::InvalidSource(
            "application MCP credential prior revision must be positive".into(),
        ));
    }
    Ok(format!(
        "application-mcp:{}:{prior_source_version}:{credential_generation}:{command_key_fingerprint}",
        source_id.0
    ))
}

fn validate_command(command: &ApplicationMcpBearerCommand) -> Result<(), CredentialError> {
    if command.source_id.0.trim().is_empty()
        || command.workspace_id.trim().is_empty()
        || command.target_fingerprint.trim().is_empty()
        || command.target_audience.trim().is_empty()
        || command.command_key_fingerprint.trim().is_empty()
        || command.credential_generation == 0
        || command.bearer.is_empty()
    {
        return Err(CredentialError::InvalidSource(
            "application MCP credential identity, idempotency, positive generation, target and bearer are required"
                .into(),
        ));
    }
    Ok(())
}

fn expected_source(
    command: &ApplicationMcpBearerCommand,
    material_ref: crate::SecretRef,
) -> CredentialSource {
    let descriptor = CredentialDescriptor::new(
        command.target_audience.clone(),
        CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
        [CredentialTargetContract::new(
            CredentialTarget::new(
                CredentialPurpose::McpAuthorization,
                command.target_audience.clone(),
            ),
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
        )],
    );
    CredentialSource {
        id: command.source_id.clone(),
        replacement_of: None,
        workspace_id: command.workspace_id.clone(),
        kind: CredentialKind::Vault,
        descriptor: Some(descriptor),
        provider_id: None,
        protocol_endpoint_id: Some(command.target_fingerprint.clone()),
        env_key: None,
        material_ref: Some(material_ref),
        auxiliary_material_refs: BTreeMap::new(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    }
}

/// Prepare an existing hosted bearer for the Managed aggregate transaction.
/// Exact replay verifies material and returns `None`; a new command returns the
/// next Source plus write-only material without publishing either row.
pub async fn prepare_application_mcp_bearer_rotation(
    command: ApplicationMcpBearerCommand,
    current: &CredentialSource,
    store: &dyn SecretStore,
) -> Result<Option<PreparedApplicationMcpBearerRotation>, CredentialError> {
    prepare_application_mcp_bearer_rotation_with_receipt_recovery(command, current, store, false)
        .await
}

/// The same admission with one legacy recovery condition: a verified exact
/// current material command has lost its historical adoption receipt. It may
/// republish once through the ordinary WAL; stale/conflicting input is still
/// rejected before this branch and can never force a rotation.
pub(crate) async fn prepare_application_mcp_bearer_rotation_with_receipt_recovery(
    command: ApplicationMcpBearerCommand,
    current: &CredentialSource,
    store: &dyn SecretStore,
    missing_receipt: bool,
) -> Result<Option<PreparedApplicationMcpBearerRotation>, CredentialError> {
    validate_command(&command)?;
    let material_ref = application_mcp_material_ref(
        &command.source_id,
        &command.command_key_fingerprint,
        command.credential_generation,
    )?;
    let expected = expected_source(&command, material_ref.clone());
    let ApplicationMcpBearerCommand {
        command_key_fingerprint,
        credential_generation,
        bearer,
        ..
    } = command;
    validate_source(current, &expected)?;
    let exact_replay = current_command_is_replay(
        current,
        &command_key_fingerprint,
        credential_generation,
        &bearer,
        store,
    )
    .await?;
    if exact_replay && current.descriptor.is_some() && !missing_receipt {
        return Ok(None);
    }
    let mut after_source = current.clone();
    after_source.version = current
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    after_source.material_ref = Some(material_ref.clone());
    after_source.descriptor = expected.descriptor;
    after_source.provider_id = None;
    Ok(Some(PreparedApplicationMcpBearerRotation {
        after_source,
        material_ref,
        bearer,
        operation_id: application_mcp_operation_id(
            &current.id,
            current.version,
            &command_key_fingerprint,
            credential_generation,
        )?,
    }))
}

/// Idempotently create or rotate one hosted application's static MCP bearer.
pub async fn enter_or_rotate_application_mcp_bearer(
    command: ApplicationMcpBearerCommand,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
    clock: &dyn WallClock,
) -> Result<CredentialSource, CredentialError> {
    validate_command(&command)?;
    let material_ref = application_mcp_material_ref(
        &command.source_id,
        &command.command_key_fingerprint,
        command.credential_generation,
    )?;
    let expected = expected_source(&command, material_ref.clone());
    let ApplicationMcpBearerCommand {
        source_id,
        command_key_fingerprint,
        credential_generation,
        bearer,
        ..
    } = command;

    let current = match repo.get(&source_id).await {
        Ok(current) => Some(current),
        Err(CredentialError::SourceNotFound(_)) => None,
        Err(error) => return Err(error),
    };
    let Some(current) = current else {
        let entered = enter_prepared_credential_idempotent(
            expected,
            Some(bearer.clone()),
            store,
            repo,
            clock,
        )
        .await?;
        verify_material(
            &entered.source,
            &command_key_fingerprint,
            credential_generation,
            &bearer,
            store,
        )
        .await?;
        return Ok(entered.source);
    };

    validate_source(&current, &expected)?;
    if current.descriptor.is_none() {
        current_command_is_replay(
            &current,
            &command_key_fingerprint,
            credential_generation,
            &bearer,
            store,
        )
        .await?;
        return migrate_legacy_provider_to_descriptor_exact(
            LegacyProviderDescriptorMigration::new(
                &source_id,
                current.version,
                APPLICATION_MCP_PROVIDER_ID,
                expected
                    .descriptor
                    .expect("application MCP expected source is described"),
                material_ref,
                bearer,
            ),
            store,
            repo,
            clock,
        )
        .await;
    }
    if current_command_is_replay(
        &current,
        &command_key_fingerprint,
        credential_generation,
        &bearer,
        store,
    )
    .await?
    {
        return Ok(current);
    }

    let descriptor = expected.descriptor;
    rotate_credential_materials_exact_with_primary_ref(
        &source_id,
        current.version,
        CredentialMaterialPatch {
            primary: Some(bearer),
            auxiliary: BTreeMap::new(),
            descriptor,
        },
        Some(material_ref),
        store,
        repo,
        clock,
    )
    .await
}

fn validate_source(
    actual: &CredentialSource,
    expected: &CredentialSource,
) -> Result<(), CredentialError> {
    let authority_matches = (actual.descriptor == expected.descriptor
        && actual.provider_id.is_none())
        || (actual.descriptor.is_none()
            && actual.provider_id.as_deref() == Some(APPLICATION_MCP_PROVIDER_ID));
    if actual.id != expected.id
        || actual.replacement_of != expected.replacement_of
        || actual.workspace_id != expected.workspace_id
        || actual.kind != CredentialKind::Vault
        || !authority_matches
        || actual.protocol_endpoint_id != expected.protocol_endpoint_id
        || actual.env_key.is_some()
        || actual.material_ref.is_none()
        || !actual.auxiliary_material_refs.is_empty()
        || actual.oauth_command.is_some()
        || actual.worker_local_binding.is_some()
        || actual.status != CredentialStatus::Active
    {
        return Err(CredentialError::MutationConflict(format!(
            "application MCP credential identity conflicts with existing source {}",
            actual.id.0
        )));
    }
    Ok(())
}

fn invalid_material_identity(source: &CredentialSource) -> CredentialError {
    CredentialError::MutationConflict(format!(
        "application MCP credential material identity conflicts with source {}",
        source.id.0
    ))
}

/// Decide the one shared replay/rotation boundary. `true` is an exact verified
/// replay; `false` admits a strictly newer generation to the caller's existing
/// WAL/CAS path. Every equal conflict or older command fails before a write.
async fn current_command_is_replay(
    current: &CredentialSource,
    command_key_fingerprint: &str,
    credential_generation: u64,
    bearer: &RedactedString,
    store: &dyn SecretStore,
) -> Result<bool, CredentialError> {
    let (current_command_fingerprint, current_generation) = material_identity(current)?;
    if current_generation == credential_generation
        && current_command_fingerprint == command_key_fingerprint
    {
        verify_material(
            current,
            command_key_fingerprint,
            credential_generation,
            bearer,
            store,
        )
        .await?;
        return Ok(true);
    }
    if credential_generation <= current_generation {
        return Err(CredentialError::MutationConflict(
            "application MCP credential generation is stale or conflicts with current material"
                .into(),
        ));
    }
    Ok(false)
}

fn material_identity(source: &CredentialSource) -> Result<(&str, u64), CredentialError> {
    let material_ref = source
        .material_ref
        .as_ref()
        .ok_or_else(|| CredentialError::MissingMaterialRef(source.id.0.clone()))?;
    let prefix = format!("sec:{}:application-mcp:", source.id.0);
    let identity = material_ref
        .0
        .strip_prefix(&prefix)
        .ok_or_else(|| invalid_material_identity(source))?;
    let (key_len, identity) = identity
        .split_once(':')
        .ok_or_else(|| invalid_material_identity(source))?;
    let key_len = key_len
        .parse::<usize>()
        .map_err(|_| invalid_material_identity(source))?;
    if key_len == 0 || identity.len() < key_len || !identity.is_char_boundary(key_len) {
        return Err(invalid_material_identity(source));
    }
    let (command_identity, suffix) = identity.split_at(key_len);
    let (credential_generation, attempt) = if suffix.is_empty() {
        (0, None)
    } else if let Some(encoded) = suffix.strip_prefix(":generation:") {
        let (generation, attempt) = encoded
            .split_once(":attempt:")
            .map_or((encoded, None), |(generation, attempt)| {
                (generation, Some(attempt))
            });
        if generation.is_empty()
            || !generation.bytes().all(|byte| byte.is_ascii_digit())
            || (generation.len() > 1 && generation.starts_with('0'))
        {
            return Err(invalid_material_identity(source));
        }
        let generation = generation
            .parse::<u64>()
            .map_err(|_| invalid_material_identity(source))?;
        if generation == 0 {
            return Err(invalid_material_identity(source));
        }
        (generation, attempt)
    } else if let Some(attempt) = suffix.strip_prefix(":attempt:") {
        (0, Some(attempt))
    } else {
        return Err(invalid_material_identity(source));
    };
    if attempt.is_some_and(|attempt| {
        attempt.is_empty() || !attempt.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return Err(invalid_material_identity(source));
    }
    Ok((command_identity, credential_generation))
}

async fn verify_material(
    source: &CredentialSource,
    command_key_fingerprint: &str,
    credential_generation: u64,
    expected: &RedactedString,
    store: &dyn SecretStore,
) -> Result<(), CredentialError> {
    if material_identity(source)? != (command_key_fingerprint, credential_generation) {
        return Err(CredentialError::MutationConflict(
            "application MCP credential command lost the concurrent create race".into(),
        ));
    }
    let material = store
        .get(source.material_ref.as_ref().expect("validated ref"))
        .await?;
    if material.expose_secret() != expected.expose_secret() {
        return Err(CredentialError::MutationConflict(
            "application MCP Idempotency-Key resolved to different sealed material".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_legacy_adoption_receipt_republishes_once_without_reopening_old_generations() {
        use crate::repo::{InMemoryCredentialRepo, ManagedCredentialRepository};
        use std::sync::Arc;
        // Causes: L1 initial generation has no predecessor event; L2 a
        // committed generation-2 event was deleted by an older binary; L3 the
        // exact command repeats; L4 stale generation or conflicting bytes.
        // Effects: L1 remains revision 1; L2 uses ordinary WAL/CAS to publish
        // revision 3 with unchanged source identity and material generation;
        // L3 reuses revision/event 3; L4 fails before material or state writes.
        // The targeted row removal represents the actual old deletion format;
        // all admission, material IO, pair publication and cleanup remain real.
        let repo = Arc::new(InMemoryCredentialRepo::new());
        let store = Arc::new(crate::InMemorySecretStore::new());
        let app = crate::VaultApplication::new(
            store.clone(),
            repo.clone(),
            awaken_time::system_clock(),
            None,
        );
        let entry = |generation, token: &str| crate::ApplicationMcpBearerEntry {
            workspace_id: "workspace".into(),
            application_authority_id: "application".into(),
            mcp_server_url: "https://flow.example.test/mcp".into(),
            target: crate::McpCredentialTarget {
                audience: "https://flow.example.test/mcp".into(),
                fingerprint: "endpoint".into(),
            },
            idempotency_key: format!("lease-{generation}"),
            credential_generation: generation,
            bearer: RedactedString::new(token),
        };
        let first = app
            .enter_application_mcp_bearer(entry(1, "first"))
            .await
            .unwrap();
        assert_eq!(first.revision, 1, "L1");
        assert_eq!(
            app.enter_application_mcp_bearer(entry(1, "first"))
                .await
                .unwrap()
                .revision,
            1,
            "L1 replay"
        );
        let second = app
            .enter_application_mcp_bearer(entry(2, "second"))
            .await
            .unwrap();
        assert_eq!(second.revision, 2);
        let lost = repo
            .pending_managed_rollouts()
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            repo.state
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                .managed_rollouts
                .remove(&lost.id)
                .is_some()
        );
        let repaired = app
            .enter_application_mcp_bearer(entry(2, "second"))
            .await
            .unwrap();
        assert_eq!(repaired.source_id, second.source_id, "L2 same authority");
        assert_eq!(repaired.revision, 3, "L2 fresh exact publication");
        let pending = repo.pending_managed_rollouts().await.unwrap();
        assert_eq!(pending.len(), 1, "L2 one new event");
        assert_eq!(pending[0].source_version, 3);
        let current = repo.get(&repaired.source_id).await.unwrap();
        assert_eq!(
            material_identity(&current).unwrap().1,
            2,
            "L2 generation unchanged"
        );
        assert_eq!(
            app.enter_application_mcp_bearer(entry(2, "second"))
                .await
                .unwrap()
                .revision,
            3,
            "L3 no second repair"
        );
        assert_eq!(
            repo.pending_managed_rollouts().await.unwrap(),
            pending,
            "L3 exact event"
        );
        let material = store.inventory().await.unwrap();
        for (generation, value) in [(1, "first"), (2, "conflicting")] {
            assert!(
                app.enter_application_mcp_bearer(entry(generation, value))
                    .await
                    .is_err(),
                "L4"
            );
            assert_eq!(
                repo.get(&repaired.source_id).await.unwrap(),
                current,
                "L4 source unchanged"
            );
            assert_eq!(
                store.inventory().await.unwrap(),
                material,
                "L4 no material writes"
            );
        }
    }
    use crate::InMemorySecretStore;
    use crate::repo::InMemoryCredentialRepo;

    #[tokio::test]
    async fn exact_legacy_replay_prepares_the_one_way_descriptor_cutover() {
        // Cause/effect graph: C1 the exact generation/material is a replay; C2
        // its persisted source still uses the legacy MCP provider marker.
        // Effects: E1 C1+!C2 is a no-op; E2 C1+C2 prepares one higher revision
        // with the canonical descriptor and removes the parallel provider_id.
        // The ordinary create/rotate tests own the non-replay rules.
        let command = ApplicationMcpBearerCommand {
            source_id: CredentialSourceId("cred:application-mcp:test".into()),
            workspace_id: "workspace-a".into(),
            target_fingerprint: "target-fingerprint".into(),
            target_audience: "https://mcp.example.test/mcp".into(),
            command_key_fingerprint: "command-fingerprint".into(),
            credential_generation: 1,
            bearer: RedactedString::new("bearer"),
        };
        let material_ref = application_mcp_material_ref(
            &command.source_id,
            &command.command_key_fingerprint,
            command.credential_generation,
        )
        .unwrap();
        let mut legacy = expected_source(&command, material_ref.clone());
        legacy.descriptor = None;
        legacy.provider_id = Some(APPLICATION_MCP_PROVIDER_ID.into());
        let store = InMemorySecretStore::new();
        store
            .put(&material_ref, command.bearer.clone())
            .await
            .unwrap();
        let repo = InMemoryCredentialRepo::new();
        repo.put(legacy.clone()).await.unwrap();

        let prepared = prepare_application_mcp_bearer_rotation(command, &legacy, &store)
            .await
            .unwrap()
            .expect("C1+C2/E2 descriptor cutover");
        assert_eq!(prepared.after_source.version, 2, "E2");
        assert!(prepared.after_source.descriptor.is_some(), "E2");
        assert!(prepared.after_source.provider_id.is_none(), "E2");
    }
}
