// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: This admin transport admits exact scoped grant commands and observations, delegates
// @formal reason: issuance to the original credential application, and projects only public receipts.
// @formal reason: It owns no token executor or durable acquisition state; route tests cover admission.
//! Thin management transport over the original credential renewal application.
use super::{
    AdminState, CredentialRenewalObservation, CredentialSourceView, Problem,
    RenewCredentialRequest, RequiredResourceWorkspace, claims_another_workspace, cred_problem,
    credential_in_scope, hosted_credential_source_id, req_id, resource_workspace_problem,
    validate_hosted_credential_identity,
};
use awaken_agent_contract::credential::CredentialRef;
use awaken_api_contract::ApiError;
use awaken_credential_contract::{
    CredentialRenewalStatus, CredentialSourceId, TokenAcquisitionError,
};
use awaken_credential_vault::{CredentialKind, CredentialSource};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

/// Application port injected by Control; the admin interface has no issuer SDK.
#[async_trait::async_trait]
pub trait CredentialGrantRenewal: Send + Sync {
    async fn inspect(
        &self,
        workspace: &str,
        source: &CredentialRef,
        min_validity_ms: u64,
    ) -> Result<Option<CredentialRenewalStatus>, TokenAcquisitionError>;

    async fn renew(
        &self,
        workspace: &str,
        source: &CredentialRef,
        replacement: CredentialSourceId,
    ) -> Result<CredentialSource, TokenAcquisitionError>;
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RenewalQuery {
    expected_version: u64,
    min_validity_ms: u64,
}

fn renewal_problem(error: TokenAcquisitionError, rid: &str) -> Problem {
    use TokenAcquisitionError as Error;
    let (status, code) = match error {
        Error::InvalidContract => (422, "invalid_credential_renewal"),
        Error::AuthorityDenied | Error::InProgress => (409, "credential_renewal_conflict"),
        Error::IssuerRejected | Error::NeedsAuthorization => {
            (409, "credential_needs_authorization")
        }
        Error::OutcomeUnknown => (409, "credential_renewal_outcome_unknown"),
        _ => (503, "credential_renewal_unavailable"),
    };
    Problem(ApiError::new(
        status,
        code,
        "Credential renewal",
        error.to_string(),
        rid,
    ))
}

fn exact_reference(
    source: &CredentialSource,
    expected: u64,
) -> Result<CredentialRef, TokenAcquisitionError> {
    if expected == 0 || u64::try_from(source.version).ok() != Some(expected) {
        return Err(TokenAcquisitionError::AuthorityDenied);
    }
    Ok(CredentialRef {
        id: source.id.0.clone(),
        revision: expected,
    })
}

pub(super) async fn inspect(
    State(state): State<AdminState>,
    RequiredResourceWorkspace(workspace): RequiredResourceWorkspace,
    Path(id): Path<String>,
    Query(query): Query<RenewalQuery>,
    headers: HeaderMap,
) -> Result<Json<CredentialRenewalObservation>, Problem> {
    let rid = req_id(&headers);
    let source = credential_in_scope(&state, &CredentialSourceId(id), &workspace, &rid).await?;
    let reference = exact_reference(&source, query.expected_version)
        .map_err(|error| renewal_problem(error, &rid))?;
    let status = if source
        .descriptor
        .as_ref()
        .is_some_and(|v| v.acquisition.is_some())
    {
        state
            .grant_renewal
            .as_ref()
            .ok_or_else(|| renewal_problem(TokenAcquisitionError::IssuerUnavailable, &rid))?
            .inspect(&workspace, &reference, query.min_validity_ms)
            .await
            .map_err(|error| renewal_problem(error, &rid))?
    } else {
        None
    };
    Ok(Json(CredentialRenewalObservation {
        workspace_id: workspace,
        reference,
        status,
    }))
}

pub(super) async fn renew(
    State(state): State<AdminState>,
    RequiredResourceWorkspace(workspace): RequiredResourceWorkspace,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RenewCredentialRequest>,
) -> Result<(StatusCode, Json<CredentialSourceView>), Problem> {
    let rid = req_id(&headers);
    if claims_another_workspace(&body.workspace_id, &workspace) {
        return Err(resource_workspace_problem(&rid));
    }
    body.descriptor
        .validate()
        .map_err(|_| renewal_problem(TokenAcquisitionError::InvalidContract, &rid))?;
    let source = credential_in_scope(&state, &CredentialSourceId(id), &workspace, &rid).await?;
    let reference = exact_reference(&source, body.expected_version)
        .map_err(|error| renewal_problem(error, &rid))?;
    if source.descriptor.as_ref() != Some(&body.descriptor) || body.descriptor.acquisition.is_none()
    {
        return Err(renewal_problem(
            TokenAcquisitionError::InvalidContract,
            &rid,
        ));
    }
    validate_hosted_credential_identity(
        &workspace,
        &body.descriptor.provider.0,
        &body.idempotency_key,
    )
    .map_err(|error| cred_problem(&error, &rid))?;
    let replacement = hosted_credential_source_id(
        &workspace,
        &body.descriptor.provider.0,
        &body.idempotency_key,
    );
    let receipt = state
        .grant_renewal
        .as_ref()
        .ok_or_else(|| renewal_problem(TokenAcquisitionError::IssuerUnavailable, &rid))?
        .renew(&workspace, &reference, replacement.clone())
        .await
        .map_err(|error| renewal_problem(error, &rid))?;
    if receipt.validate_authority().is_err()
        || receipt.kind != CredentialKind::Vault
        || receipt.id != replacement
        || receipt.id.0 == reference.id
        || receipt.version != 1
        || receipt.workspace_id != workspace
        || receipt.replacement_of.as_ref() != Some(&reference)
        || receipt.descriptor.as_ref() != Some(&body.descriptor)
        || receipt.status != awaken_credential_vault::CredentialStatus::Active
    {
        return Err(renewal_problem(
            TokenAcquisitionError::PublicationFailed,
            &rid,
        ));
    }
    Ok((StatusCode::CREATED, Json(receipt.into())))
}
