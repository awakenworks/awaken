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
    pub bearer: RedactedString,
}

pub struct PreparedApplicationMcpBearerRotation {
    pub after_source: CredentialSource,
    pub material_ref: crate::SecretRef,
    pub bearer: RedactedString,
    pub operation_id: String,
}

/// Prepare an existing hosted bearer for the Managed aggregate transaction.
/// Exact replay verifies material and returns `None`; a new command returns the
/// next Source plus write-only material without publishing either row.
pub async fn prepare_application_mcp_bearer_rotation(
    command: ApplicationMcpBearerCommand,
    current: &CredentialSource,
    store: &dyn SecretStore,
) -> Result<Option<PreparedApplicationMcpBearerRotation>, CredentialError> {
    let ApplicationMcpBearerCommand {
        source_id,
        workspace_id,
        target_fingerprint,
        command_key_fingerprint,
        bearer,
    } = command;
    if source_id.0.trim().is_empty()
        || workspace_id.trim().is_empty()
        || target_fingerprint.trim().is_empty()
        || command_key_fingerprint.trim().is_empty()
        || bearer.is_empty()
    {
        return Err(CredentialError::InvalidSource(
            "application MCP credential identity, idempotency, target and bearer are required"
                .into(),
        ));
    }
    let material_ref = crate::SecretRef(format!(
        "sec:{}:application-mcp:{}:{command_key_fingerprint}",
        source_id.0,
        command_key_fingerprint.len(),
    ));
    let expected = CredentialSource {
        id: source_id,
        workspace_id,
        kind: CredentialKind::Vault,
        provider_id: Some(APPLICATION_MCP_PROVIDER_ID.to_owned()),
        protocol_endpoint_id: Some(target_fingerprint),
        env_key: None,
        material_ref: Some(material_ref.clone()),
        auxiliary_material_refs: BTreeMap::new(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    };
    validate_source(current, &expected)?;
    if material_identity(current)? == command_key_fingerprint {
        verify_material(current, &command_key_fingerprint, &bearer, store).await?;
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
        operation_id: format!(
            "application-mcp:{}:{}:{}",
            current.id.0, current.version, command_key_fingerprint
        ),
    }))
}

/// Idempotently create or rotate one hosted application's static MCP bearer.
pub async fn enter_or_rotate_application_mcp_bearer(
    command: ApplicationMcpBearerCommand,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let ApplicationMcpBearerCommand {
        source_id,
        workspace_id,
        target_fingerprint,
        command_key_fingerprint,
        bearer,
    } = command;
    if source_id.0.trim().is_empty()
        || workspace_id.trim().is_empty()
        || target_fingerprint.trim().is_empty()
        || command_key_fingerprint.trim().is_empty()
        || bearer.is_empty()
    {
        return Err(CredentialError::InvalidSource(
            "application MCP credential identity, idempotency, target and bearer are required"
                .into(),
        ));
    }
    let material_ref = crate::SecretRef(format!(
        "sec:{}:application-mcp:{}:{command_key_fingerprint}",
        source_id.0,
        command_key_fingerprint.len(),
    ));
    let expected = CredentialSource {
        id: source_id.clone(),
        workspace_id,
        kind: CredentialKind::Vault,
        provider_id: Some(APPLICATION_MCP_PROVIDER_ID.to_owned()),
        protocol_endpoint_id: Some(target_fingerprint),
        env_key: None,
        material_ref: Some(material_ref.clone()),
        auxiliary_material_refs: BTreeMap::new(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    };

    let current = match repo.get(&source_id).await {
        Ok(current) => Some(current),
        Err(CredentialError::SourceNotFound(_)) => None,
        Err(error) => return Err(error),
    };
    let Some(current) = current else {
        let entered =
            enter_prepared_credential_idempotent(expected, Some(bearer.clone()), store, repo)
                .await?;
        verify_material(&entered.source, &command_key_fingerprint, &bearer, store).await?;
        return Ok(entered.source);
    };

    validate_source(&current, &expected)?;
    if material_identity(&current)? == command_key_fingerprint {
        verify_material(&current, &command_key_fingerprint, &bearer, store).await?;
        return Ok(current);
    }

    rotate_credential_materials_exact_with_primary_ref(
        &source_id,
        current.version,
        CredentialMaterialPatch {
            primary: Some(bearer),
            auxiliary: BTreeMap::new(),
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

fn material_identity(source: &CredentialSource) -> Result<&str, CredentialError> {
    let material_ref = source
        .material_ref
        .as_ref()
        .ok_or_else(|| CredentialError::MissingMaterialRef(source.id.0.clone()))?;
    let prefix = format!("sec:{}:application-mcp:", source.id.0);
    let identity = material_ref.0.strip_prefix(&prefix).ok_or_else(|| {
        CredentialError::MutationConflict(format!(
            "application MCP credential material identity conflicts with source {}",
            source.id.0
        ))
    })?;
    let (key_len, identity) = identity.split_once(':').ok_or_else(|| {
        CredentialError::MutationConflict(format!(
            "application MCP credential material identity conflicts with source {}",
            source.id.0
        ))
    })?;
    let key_len = key_len.parse::<usize>().map_err(|_| {
        CredentialError::MutationConflict(format!(
            "application MCP credential material identity conflicts with source {}",
            source.id.0
        ))
    })?;
    if key_len == 0 || identity.len() < key_len || !identity.is_char_boundary(key_len) {
        return Err(CredentialError::MutationConflict(format!(
            "application MCP credential material identity conflicts with source {}",
            source.id.0
        )));
    }
    let (command_identity, suffix) = identity.split_at(key_len);
    if !suffix.is_empty() {
        let attempt = suffix.strip_prefix(":attempt:").ok_or_else(|| {
            CredentialError::MutationConflict(format!(
                "application MCP credential material identity conflicts with source {}",
                source.id.0
            ))
        })?;
        if attempt.is_empty() || !attempt.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(CredentialError::MutationConflict(format!(
                "application MCP credential material identity conflicts with source {}",
                source.id.0
            )));
        }
    }
    Ok(command_identity)
}

async fn verify_material(
    source: &CredentialSource,
    command_key_fingerprint: &str,
    expected: &RedactedString,
    store: &dyn SecretStore,
) -> Result<(), CredentialError> {
    if material_identity(source)? != command_key_fingerprint {
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
