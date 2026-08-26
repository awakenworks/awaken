//! Hosted application MCP bearer commands over the credential aggregate.

use std::collections::BTreeMap;

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialSourceId;

use super::{
    CredentialMaterialPatch, CredentialRepo, enter_prepared_credential_idempotent,
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
    CredentialSource {
        id: command.source_id.clone(),
        workspace_id: command.workspace_id.clone(),
        kind: CredentialKind::Vault,
        descriptor: None,
        provider_id: Some(APPLICATION_MCP_PROVIDER_ID.to_owned()),
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
    if current_command_is_replay(
        current,
        &command_key_fingerprint,
        credential_generation,
        &bearer,
        store,
    )
    .await?
    {
        return Ok(None);
    }
    let mut after_source = current.clone();
    after_source.version = current
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    after_source.material_ref = Some(material_ref.clone());
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
        let entered =
            enter_prepared_credential_idempotent(expected, Some(bearer.clone()), store, repo)
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

    rotate_credential_materials_exact_with_primary_ref(
        &source_id,
        current.version,
        CredentialMaterialPatch {
            primary: Some(bearer),
            auxiliary: BTreeMap::new(),
            descriptor: None,
        },
        Some(material_ref),
        store,
        repo,
    )
    .await
}

fn validate_source(
    actual: &CredentialSource,
    expected: &CredentialSource,
) -> Result<(), CredentialError> {
    if actual.id != expected.id
        || actual.workspace_id != expected.workspace_id
        || actual.kind != CredentialKind::Vault
        || actual.provider_id.as_deref() != Some(APPLICATION_MCP_PROVIDER_ID)
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
