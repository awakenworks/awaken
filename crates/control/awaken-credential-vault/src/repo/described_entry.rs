// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Described grant entry reuses the source material WAL and exact replay checks.
//! One described entry path for scalar credentials and declared renewable grants.
use super::{
    BTreeMap, CredentialCreateParams, CredentialEntry, CredentialError, CredentialRepo,
    CredentialSourceId, CredentialStatus, SecretStore, TOKEN_ACQUISITION_STATE_SLOT,
    TokenPublicationState, credential_operations,
};
use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialGrantLifetime;
use awaken_time::WallClock;

/// Write-only request. No serde/Debug: auxiliary tokens cannot become facts.
pub struct DescribedCredentialEntryCommand {
    pub id: CredentialSourceId,
    pub params: CredentialCreateParams,
    pub protocol_endpoint_id: Option<String>,
    pub descriptor: awaken_credential_contract::CredentialDescriptor,
    pub replacement_of: Option<awaken_agent_contract::credential::CredentialRef>,
    pub auxiliary_materials: BTreeMap<String, RedactedString>,
    pub lifetime: Option<CredentialGrantLifetime>,
}

pub async fn enter_described_credential(
    command: DescribedCredentialEntryCommand,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
    clock: &dyn WallClock,
) -> Result<CredentialEntry, CredentialError> {
    let DescribedCredentialEntryCommand {
        id,
        params,
        protocol_endpoint_id,
        descriptor,
        replacement_of,
        mut auxiliary_materials,
        lifetime,
    } = command;
    let (mut expected, secret) = credential_operations::prepare_described_source(
        Some(id),
        params,
        protocol_endpoint_id,
        descriptor,
    )?;
    if let Some(profile) = expected
        .descriptor
        .as_ref()
        .and_then(|v| v.acquisition.as_ref())
    {
        if auxiliary_materials
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            != profile.auxiliary_slots()
        {
            return Err(CredentialError::InvalidSource(
                "grant material slots must exactly match its acquisition profile".into(),
            ));
        }
        if let Some(lifetime) = lifetime {
            if [
                lifetime.access_expires_at_unix_ms,
                lifetime.refresh_expires_at_unix_ms,
            ]
            .into_iter()
            .flatten()
            .any(|at| at <= lifetime.issued_at_unix_ms)
                || (lifetime.refresh_expires_at_unix_ms.is_some()
                    && !auxiliary_materials.contains_key(crate::OAUTH_REFRESH_TOKEN_SLOT))
            {
                return Err(CredentialError::InvalidSource(
                    "grant lifetime is inconsistent with its material".into(),
                ));
            }
            let state = TokenPublicationState {
                version: 1,
                predecessor: None,
                successor: awaken_agent_contract::credential::CredentialRef {
                    id: expected.id.0.clone(),
                    revision: 1,
                },
                profile_fingerprint: profile.fingerprint().map_err(|_| {
                    CredentialError::InvalidSource("invalid acquisition profile".into())
                })?,
                issued_at_unix_ms: lifetime.issued_at_unix_ms,
                access_expires_at_unix_ms: lifetime.access_expires_at_unix_ms,
                refresh_expires_at_unix_ms: lifetime.refresh_expires_at_unix_ms,
            };
            auxiliary_materials.insert(
                TOKEN_ACQUISITION_STATE_SLOT.into(),
                RedactedString::new(
                    serde_json::to_string(&state).map_err(|_| {
                        CredentialError::InvalidSource("invalid grant state".into())
                    })?,
                ),
            );
        }
    } else if !auxiliary_materials.is_empty() || lifetime.is_some() {
        return Err(CredentialError::InvalidSource(
            "auxiliary grant material requires a declared acquisition profile".into(),
        ));
    }
    for slot in auxiliary_materials.keys() {
        expected.auxiliary_material_refs.insert(
            slot.clone(),
            credential_operations::material_ref_for(&expected.id, expected.version, slot),
        );
    }
    expected.replacement_of = replacement_of.clone();
    expected.validate_authority()?;
    // Durable replay is checked before predecessor lifecycle: relink+retirement
    // may already have completed after the original acknowledgement was lost.
    let before = match repo.get(&expected.id).await {
        Ok(_) => None,
        Err(CredentialError::SourceNotFound(_)) => {
            if let Some(reference) = replacement_of {
                if reference.id == expected.id.0 || reference.revision == 0 {
                    return Err(CredentialError::InvalidSource(
                        "replacement needs a distinct positive predecessor".into(),
                    ));
                }
                let before = repo.get(&CredentialSourceId(reference.id.clone())).await?;
                if before.workspace_id != expected.workspace_id {
                    return Err(CredentialError::SourceNotFound(reference.id));
                }
                if u64::try_from(before.version).ok() != Some(reference.revision)
                    || before.status != CredentialStatus::Active
                    || before.descriptor.as_ref().map(|v| &v.provider)
                        != expected.descriptor.as_ref().map(|v| &v.provider)
                {
                    return Err(CredentialError::MutationConflict(
                        "replacement predecessor changed or is outside the declared scope".into(),
                    ));
                }
                if before
                    .descriptor
                    .as_ref()
                    .is_some_and(|v| v.acquisition.is_none())
                    && !before.auxiliary_material_refs.is_empty()
                {
                    return Err(CredentialError::InvalidSource(
                        "primary-only predecessor cannot acquire implicit auxiliary authority"
                            .into(),
                    ));
                }
                Some(before)
            } else {
                None
            }
        }
        Err(e) => return Err(e),
    };
    let expected_material = secret.clone();
    let expected_auxiliary = auxiliary_materials.clone();
    let entry = credential_operations::enter_prepared_credential_idempotent_with_before(
        expected,
        secret,
        auxiliary_materials,
        before,
        store,
        repo,
        clock,
    )
    .await?;
    let entry =
        credential_operations::verify_idempotent_material(entry, expected_material.as_ref(), store)
            .await?;
    for (slot, value) in expected_auxiliary {
        let reference = entry.source.auxiliary_material_ref(&slot).ok_or_else(|| {
            CredentialError::MutationConflict("grant replay removed an auxiliary slot".into())
        })?;
        if store.get(reference).await?.expose_secret() != value.expose_secret() {
            return Err(CredentialError::MutationConflict(
                "grant idempotency identity was reused with different private material".into(),
            ));
        }
    }
    Ok(entry)
}
