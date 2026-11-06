// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Token publication extends the existing material WAL and never introduces a second credential store.
//! Generic, pre-dispatch-fenced token publication over the existing source WAL.
use super::{
    CredentialAcquisitionPhase, CredentialError, CredentialKind, CredentialMutationIntent,
    CredentialRepo, CredentialSource, CredentialSourceId, CredentialStatus, SecretStore,
    credential_operations,
};
use awaken_agent_contract::{RedactedString, StructuredCredentialMaterial};
use awaken_credential_contract::{
    AcquiredCredentialToken, CredentialMaterialDescriptor, IssuedCredentialTokens,
};
use awaken_time::{WallClock, WallClockExt as _};

pub use awaken_credential_contract::TOKEN_ACQUISITION_STATE_SLOT;

pub async fn recover_token_publication(
    intent: &CredentialMutationIntent,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
    clock: &dyn WallClock,
) -> Result<bool, CredentialError> {
    if intent.acquisition.is_none() {
        return Err(CredentialError::InvalidSource(
            "not a token publication".into(),
        ));
    }
    credential_operations::recover_credential_mutation(intent, store, repo, clock.now_ms()).await
}

pub async fn abandon_token_publication(
    intent: &CredentialMutationIntent,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<(), CredentialError> {
    let phase = intent.acquisition.as_ref().map(|v| v.phase);
    if phase != Some(CredentialAcquisitionPhase::ConfirmedUnsent) {
        return Err(CredentialError::MutationConflict(
            "token dispatch cannot be discarded without reconciliation or new authorization".into(),
        ));
    }
    let reclaiming = repo.abort_mutation(intent).await?;
    credential_operations::reclaim_and_complete_source_mutation(&reclaiming, store, repo).await
}

/// Metadata belongs to the same sealed generation as its access/refresh pair.
/// It contains no token bytes and proves an exact predecessor, not read-latest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenPublicationState {
    pub version: u32,
    pub predecessor: Option<awaken_agent_contract::credential::CredentialRef>,
    pub successor: awaken_agent_contract::credential::CredentialRef,
    pub profile_fingerprint: String,
    pub issued_at_unix_ms: u64,
    pub access_expires_at_unix_ms: Option<u64>,
    pub refresh_expires_at_unix_ms: Option<u64>,
}

pub async fn token_publication_state(
    source: &CredentialSource,
    store: &dyn SecretStore,
) -> Result<Option<TokenPublicationState>, CredentialError> {
    let Some(reference) = source.auxiliary_material_ref(TOKEN_ACQUISITION_STATE_SLOT) else {
        return Ok(None);
    };
    let value = store.get(reference).await?;
    let state: TokenPublicationState =
        serde_json::from_str(value.expose_secret()).map_err(|_| {
            CredentialError::InvalidSource("invalid sealed token publication state".into())
        })?;
    if state.version != 1
        || state.successor.id != source.id.0
        || i64::try_from(state.successor.revision).ok() != Some(source.version)
    {
        return Err(CredentialError::InvalidSource(
            "token publication state differs from its source generation".into(),
        ));
    }
    Ok(Some(state))
}

/// Claim before contacting a rotating token issuer. A replacement id is
/// deterministic caller intent; business owners still serialize cross-id
/// replacements through their existing canonical Credential Resource claim.
pub async fn claim_token_publication(
    source: &CredentialSource,
    replacement_id: Option<CredentialSourceId>,
    profile_fingerprint: String,
    publish_refresh: bool,
    repo: &dyn CredentialRepo,
    clock: &dyn WallClock,
) -> Result<CredentialMutationIntent, CredentialError> {
    source.validate_authority()?;
    if source.status != CredentialStatus::Active || source.kind != CredentialKind::Vault {
        return Err(CredentialError::NotActive(source.id.0.clone()));
    }
    let mut after = source.clone();
    if let Some(id) = replacement_id {
        if id == source.id {
            return Err(CredentialError::InvalidSource(
                "token replacement requires a distinct source".into(),
            ));
        }
        after.id = id;
        after.version = 1;
        after.replacement_of = Some(awaken_agent_contract::credential::CredentialRef {
            id: source.id.0.clone(),
            revision: u64::try_from(source.version)
                .map_err(|_| CredentialError::InvalidSource("invalid source revision".into()))?,
        });
        for (slot, reference) in &mut after.auxiliary_material_refs {
            *reference = credential_operations::material_ref_for(&after.id, after.version, slot);
        }
    } else {
        after.version = source.version.checked_add(1).ok_or_else(|| {
            CredentialError::InvalidSource("token source revision overflow".into())
        })?;
    }
    after.material_ref = Some(credential_operations::material_ref_for(
        &after.id,
        after.version,
        "primary",
    ));
    if !publish_refresh {
        after
            .auxiliary_material_refs
            .remove(crate::OAUTH_REFRESH_TOKEN_SLOT);
    }
    let slots = std::iter::once(TOKEN_ACQUISITION_STATE_SLOT)
        .chain(publish_refresh.then_some(crate::OAUTH_REFRESH_TOKEN_SLOT));
    for slot in slots {
        after.auxiliary_material_refs.insert(
            slot.into(),
            credential_operations::material_ref_for(&after.id, after.version, slot),
        );
    }
    let intent = CredentialMutationIntent::prepare(Some(source.clone()), after, clock.now_ms())?
        .with_acquisition(profile_fingerprint)?;
    if !repo.begin_mutation(intent.clone(), clock.now_ms()).await? {
        return Err(CredentialError::MutationConflict(
            "token acquisition is already pending".into(),
        ));
    }
    Ok(intent)
}

fn encode_primary(
    source: &CredentialSource,
    field: Option<&str>,
    access: RedactedString,
    previous: &RedactedString,
) -> Result<RedactedString, CredentialError> {
    let material = match source.descriptor.as_ref().map(|v| &v.material) {
        Some(CredentialMaterialDescriptor::Structured { type_id, fields }) => {
            let field = field.ok_or_else(|| {
                CredentialError::InvalidSource(
                    "structured token output needs an exact field".into(),
                )
            })?;
            if !fields.contains(field) {
                return Err(CredentialError::InvalidSource(
                    "token output field is not declared".into(),
                ));
            }
            let mut material =
                crate::decode_structured_material(previous.clone())?.map_err(|_| {
                    CredentialError::InvalidSource("token source material is not structured".into())
                })?;
            if material.type_id != type_id.0 {
                return Err(CredentialError::InvalidSource(
                    "token source material type changed".into(),
                ));
            }
            material.fields.insert(field.into(), access);
            crate::encode_structured_material(StructuredCredentialMaterial {
                type_id: material.type_id,
                fields: material.fields,
            })?
        }
        _ => access,
    };
    if let Some(descriptor) = source.descriptor.as_ref() {
        crate::validate_described_material(descriptor, &material)?;
    }
    Ok(material)
}

/// Seal the complete pair and lifetime state, then use the ORIGINAL source
/// publication/reclamation implementation. No credential material is serialized
/// onto a repository fact; only the opaque material references are durable there.
pub async fn publish_acquired_tokens(
    intent: CredentialMutationIntent,
    tokens: IssuedCredentialTokens,
    access_field: Option<&str>,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
    clock: &dyn WallClock,
) -> Result<(CredentialSource, AcquiredCredentialToken), CredentialError> {
    intent.validate()?;
    let acquisition = intent
        .acquisition
        .as_ref()
        .ok_or_else(|| CredentialError::InvalidSource("missing acquisition intent".into()))?;
    if acquisition.phase != CredentialAcquisitionPhase::Dispatched
        || intent.material_fence.writer_lease_expires_at_unix_ms <= clock.now_ms()
    {
        return Err(CredentialError::MutationConflict(
            "token publication writer lost its live lease".into(),
        ));
    }
    let before = intent.before.as_ref().ok_or_else(|| {
        CredentialError::InvalidSource("token publication has no predecessor".into())
    })?;
    if repo.get(&before.id).await? != *before {
        return Err(CredentialError::MutationConflict(
            "token source changed during acquisition".into(),
        ));
    }
    let previous = store
        .get(
            before
                .material_ref
                .as_ref()
                .ok_or_else(|| CredentialError::MissingMaterialRef(before.id.0.clone()))?,
        )
        .await?;
    let access = tokens.access_token.clone();
    let primary = encode_primary(before, access_field, tokens.access_token, &previous)?;
    let refresh = if intent
        .after
        .auxiliary_material_ref(crate::OAUTH_REFRESH_TOKEN_SLOT)
        .is_some()
    {
        Some(if let Some(refresh) = tokens.refresh_token {
            refresh
        } else {
            store
                .get(
                    before
                        .auxiliary_material_ref(crate::OAUTH_REFRESH_TOKEN_SLOT)
                        .ok_or_else(|| {
                            CredentialError::InvalidSource(
                                "token source has no refresh material".into(),
                            )
                        })?,
                )
                .await?
        })
    } else {
        None
    };
    let old_state = token_publication_state(before, store).await?;
    let state = TokenPublicationState {
        version: 1,
        predecessor: Some(awaken_agent_contract::credential::CredentialRef {
            id: before.id.0.clone(),
            revision: u64::try_from(before.version)
                .map_err(|_| CredentialError::InvalidSource("invalid source revision".into()))?,
        }),
        successor: awaken_agent_contract::credential::CredentialRef {
            id: intent.after.id.0.clone(),
            revision: u64::try_from(intent.after.version).map_err(|_| {
                CredentialError::InvalidSource("invalid token successor revision".into())
            })?,
        },
        profile_fingerprint: acquisition.profile_fingerprint.clone(),
        issued_at_unix_ms: clock.now_ms(),
        access_expires_at_unix_ms: tokens.access_expires_at_unix_ms,
        refresh_expires_at_unix_ms: if refresh.is_some() {
            tokens
                .refresh_expires_at_unix_ms
                .or_else(|| old_state.and_then(|v| v.refresh_expires_at_unix_ms))
        } else {
            None
        },
    };
    let metadata = serde_json::to_string(&state)
        .map(RedactedString::new)
        .map_err(|_| {
            CredentialError::InvalidSource("token publication metadata cannot be encoded".into())
        })?;
    let refs = &intent.after;
    let mut materials = vec![
        (
            refs.material_ref
                .clone()
                .ok_or_else(|| CredentialError::MissingMaterialRef(refs.id.0.clone()))?,
            primary,
        ),
        (
            refs.auxiliary_material_ref(TOKEN_ACQUISITION_STATE_SLOT)
                .cloned()
                .ok_or_else(|| {
                    CredentialError::InvalidSource("missing planned token state".into())
                })?,
            metadata,
        ),
    ];
    if let Some(refresh) = refresh {
        materials.push((
            refs.auxiliary_material_ref(crate::OAUTH_REFRESH_TOKEN_SLOT)
                .cloned()
                .ok_or_else(|| {
                    CredentialError::InvalidSource("missing planned refresh material".into())
                })?,
            refresh,
        ));
    }
    for (slot, reference) in &refs.auxiliary_material_refs {
        if matches!(
            slot.as_str(),
            crate::OAUTH_REFRESH_TOKEN_SLOT | TOKEN_ACQUISITION_STATE_SLOT
        ) {
            continue;
        }
        if let Some(previous) = before.auxiliary_material_ref(slot)
            && previous != reference
        {
            materials.push((reference.clone(), store.get(previous).await?));
        }
    }
    let committed =
        credential_operations::complete_owned_source_mutation(intent, Some(store), materials, repo)
            .await?;
    let token = AcquiredCredentialToken {
        additional_fields: Default::default(),
        access_token: access,
        expires_at_unix_ms: state.access_expires_at_unix_ms,
        reference: Some(state.successor),
        cache_generation: None,
        cache_key: None,
        cache_partition: None,
    };
    Ok((committed.after, token))
}
