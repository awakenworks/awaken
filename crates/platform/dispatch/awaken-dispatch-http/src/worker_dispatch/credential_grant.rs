// @formal entrypoint credential.attempt_delivery_is_exact_epoch_bound_and_authority_preserving
// @formal evidence worker.secret_materialization_is_claim_fenced
//! The one grant-addressed credential endpoint of a registered Worker.
//!
//! Every grant kind is admitted by the same rule
//! ([`awaken_dispatch::credential_grant_admission`]): the authenticated current
//! incarnation must hold the lease it names, the lease must be its owning
//! aggregate's current live lease, and the grant must be frozen under that
//! lease. Run grants are judged by the Dispatch claim fence and the claimed
//! Run's frozen candidates; MCP bearer grants by the Session aggregate's MCP
//! realization law. After issuance the holding is re-read: material issued to
//! a lost holding is never returned (a brokered grant is closed instead).

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_agent_contract::credential::{AuthChallenge, CandidateFingerprint, Credential};
use awaken_credential_contract::{
    AttemptCredentialBinding, AttemptCredentialDeliveryRequest, BrokeredGrantOperation,
    BrokeredModelAccess, ClaimCredentialGrant, ClaimCredentialGrantMaterial, GrantedSecret,
};
use awaken_dispatch_contract::{
    CredentialGrantAdmission, CredentialGrantKind, CredentialGrantRequest, RunDispatch,
    WorkerCredentialGrant, WorkerCredentialGrantMaterial, credential_grant_admission,
};
use awaken_runtime_contract::resolved::ResolvedModelCandidate;
use awaken_session_configuration_contract::mcp::{McpCredentialGrantRefusal, McpGenerationRef};

/// The Coordinator's issuers, one per grant kind. An absent issuer refuses
/// its kind as unavailable; it never falls back to another kind.
#[derive(Clone, Default)]
pub struct WorkerCredentialGrantIssuers {
    /// Recipient-sealed Provider deliveries (present only where Control signs
    /// claim-scoped carriers).
    pub provider: Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
    /// The deployment's authenticated broker identity.
    pub brokered: Option<Arc<dyn awaken_credential_contract::BrokeredGrantIssuer>>,
    /// The Vault refresh authority of pinned MCP bearers.
    pub mcp: Option<Arc<dyn awaken_session_configuration_contract::mcp::McpCredentialRefresh>>,
}

/// A brokered grant this Coordinator issued, retained only until it expires
/// or is closed, so that renew and close are admitted only to the holder of
/// the claim and candidate it was issued for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct IssuedBrokeredGrant {
    claim: RunClaim,
    candidate: CandidateFingerprint,
    expires_at_unix_s: i64,
}

/// Brokered grant expiry is in Unix seconds; keep one minute of slack so a
/// renew racing expiry is still judged by the broker, not dropped here.
const BROKERED_LEDGER_SLACK_S: i64 = 60;

/// The longest lifetime of a brokered grant this Coordinator returns. Brokered
/// grants are short-lived per-request Gateway capabilities; the ledger that admits renew/close is process memory, so
/// a grant whose close is refused after a Coordinator restart must still end
/// on the broker's clock. It is held only by the attempt it was issued to.
const MAX_BROKERED_GRANT_LIFETIME_S: i64 = 15 * 60;

/// | Rule | grant expiry (Unix s) | effect |
/// |---|---|---|
/// | L1 | after now + bound | refused: released at once, never returned |
/// | L2 | within now + bound (incl. already past) | returned |
fn brokered_grant_lifetime_is_bounded(expires_at_unix_s: i64, now_ms: u64) -> bool {
    let now_s = i64::try_from(now_ms / 1_000).unwrap_or(i64::MAX);
    expires_at_unix_s <= now_s.saturating_add(MAX_BROKERED_GRANT_LIFETIME_S)
}

fn prune_brokered_ledger(
    ledger: &mut std::collections::BTreeMap<String, IssuedBrokeredGrant>,
    now_ms: u64,
) {
    let now_s = i64::try_from(now_ms / 1_000).unwrap_or(i64::MAX);
    ledger.retain(|_, issued| {
        issued
            .expires_at_unix_s
            .saturating_add(BROKERED_LEDGER_SLACK_S)
            > now_s
    });
}

/// The single response mapping of an admission refusal. A stale holding has
/// no material (`null`), exactly like a holding lost during issuance. A
/// missing issuer is a typed, terminal answer naming the kind; a Coordinator
/// without a broker answers brokered grants exactly as one whose brokered
/// supply is disabled.
fn refused(
    admission: CredentialGrantAdmission,
    kind: CredentialGrantKind,
) -> Result<Option<WorkerCredentialGrantMaterial>, ApplicationError> {
    match admission {
        CredentialGrantAdmission::Issue => Err(ApplicationError::internal(
            "an issuable grant was routed as a refusal",
        )),
        CredentialGrantAdmission::ForeignHolder => Err(ApplicationError::invalid(
            "authenticated Worker does not hold the credential grant's lease",
        )),
        CredentialGrantAdmission::StaleHolding => Ok(None),
        CredentialGrantAdmission::GrantNotHeld => Err(ApplicationError::invalid(
            "the lease holds no such credential grant",
        )),
        CredentialGrantAdmission::IssuerMissing => Ok(Some(match kind {
            CredentialGrantKind::BrokeredModel => WorkerCredentialGrantMaterial::Claim(
                ClaimCredentialGrantMaterial::BrokeredModelRefused(
                    awaken_credential_contract::BrokeredGrantRefusal::CloudModelsDisabled,
                ),
            ),
            kind => WorkerCredentialGrantMaterial::IssuerMissing(kind),
        })),
    }
}

fn claim_material(
    material: Option<ClaimCredentialGrantMaterial>,
) -> Option<WorkerCredentialGrantMaterial> {
    material.map(WorkerCredentialGrantMaterial::Claim)
}

pub(super) async fn credential_grant(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<CredentialGrantRequest>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let authority = claim_authority(&service, &worker, Some(&request.identity), false).await?;
        let material = match request.grant {
            WorkerCredentialGrant::Claim { claim, grant } => {
                claim_grant(&service, &authority, &claim, grant).await?
            }
            WorkerCredentialGrant::McpGeneration {
                generation,
                credential,
                challenge_status,
                www_authenticate,
            } => {
                mcp_generation_grant(
                    &service,
                    &authority,
                    &generation,
                    &credential,
                    AuthChallenge {
                        status: challenge_status,
                        www_authenticate,
                    },
                )
                .await?
            }
        };
        Ok(json!({ "material": material }))
    }
    .await;
    respond(result)
}

/// The claimed Run's frozen request while `claim` is its current live owner.
/// Liveness is the Dispatch store's own judgement (`claim_is_current`), on the
/// clock its rows are leased by; the commit-epoch guard only reads the exact
/// owner/epoch row and is released before any issuer I/O.
async fn claimed_request(
    service: &WorkerDispatchService,
    claim: &RunClaim,
) -> Result<Option<RunDispatch>, ApplicationError> {
    let Some(guard) = service
        .dispatch
        .lock_commit_epoch(claim)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
    else {
        return Ok(None);
    };
    let request = (guard.request().run_id() == &claim.run_id).then(|| guard.request().clone());
    guard
        .release()
        .await
        .map_err(ApplicationError::unavailable)?;
    let current = service
        .dispatch
        .claim_is_current(claim)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
    Ok(request.filter(|_| current))
}

fn frozen_candidate<'a>(
    request: &'a RunDispatch,
    fingerprint: &CandidateFingerprint,
) -> Option<&'a ResolvedModelCandidate> {
    request
        .activation
        .snapshot
        .resolved_spec
        .attempt_candidates(request.activation.model_ref_override.as_deref())
        .into_iter()
        .find(|candidate| {
            awaken_credential_contract::candidate_fingerprint(candidate)
                .is_ok_and(|candidate| &candidate == fingerprint)
        })
}

/// Compile the exact delivery request of one frozen binding from the frozen
/// candidate. Claim attachment and grant re-delivery share this compiler, so
/// a Worker never supplies a holder, material binding or Provider route.
pub(super) fn provider_delivery_request(
    candidate: &ResolvedModelCandidate,
    binding: &AttemptCredentialBinding,
    claim: &RunClaim,
) -> Result<Option<AttemptCredentialDeliveryRequest>, ApplicationError> {
    let Some((published_access, material_binding)) =
        awaken_credential_contract::candidate_credential_material_binding(candidate)
    else {
        return Ok(None);
    };
    Ok(Some(AttemptCredentialDeliveryRequest {
        candidate_fingerprint: binding.candidate_fingerprint.clone(),
        published_access: published_access.clone(),
        selected_plaintext_holder: binding.selected_plaintext_holder.clone(),
        binding: material_binding,
        expected_provider: awaken_credential_contract::candidate_provider_expectation(candidate)
            .map_err(ApplicationError::internal)?,
        claim_epoch: claim.epoch,
    }))
}

async fn claim_grant(
    service: &WorkerDispatchService,
    authority: &ClaimAuthority,
    claim: &RunClaim,
    grant: ClaimCredentialGrant,
) -> Result<Option<WorkerCredentialGrantMaterial>, ApplicationError> {
    let holder_is_caller = authority.owner == claim.owner;
    let request = if holder_is_caller {
        claimed_request(service, claim).await?
    } else {
        None
    };
    let material = match grant {
        ClaimCredentialGrant::ProviderCredential {
            candidate_fingerprint,
        } => {
            provider_credential(
                service,
                claim,
                holder_is_caller,
                request.as_ref(),
                &candidate_fingerprint,
            )
            .await?
        }
        ClaimCredentialGrant::BrokeredModel {
            candidate_fingerprint,
            operation,
        } => {
            brokered_model(
                service,
                authority,
                claim,
                holder_is_caller,
                request.as_ref(),
                candidate_fingerprint,
                operation,
            )
            .await?
        }
    };
    Ok(material)
}

async fn provider_credential(
    service: &WorkerDispatchService,
    claim: &RunClaim,
    holder_is_caller: bool,
    request: Option<&RunDispatch>,
    fingerprint: &CandidateFingerprint,
) -> Result<Option<WorkerCredentialGrantMaterial>, ApplicationError> {
    let binding = match request {
        Some(_) => service
            .dispatch
            .current_credential_binding(claim, fingerprint)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?,
        None => None,
    };
    let delivery_request = match (request, &binding) {
        (Some(request), Some(binding)) => frozen_candidate(request, fingerprint)
            .map(|candidate| provider_delivery_request(candidate, binding, claim))
            .transpose()?
            .flatten(),
        _ => None,
    };
    let source = service.credential_grants.provider.as_deref();
    let admission = credential_grant_admission(
        holder_is_caller,
        request.is_some(),
        delivery_request.is_some(),
        source.is_some(),
    );
    let (Some(source), Some(binding), Some(delivery_request)) = (source, binding, delivery_request)
    else {
        return refused(admission, CredentialGrantKind::ProviderCredential);
    };
    if admission != CredentialGrantAdmission::Issue {
        return refused(admission, CredentialGrantKind::ProviderCredential);
    }
    let delivery = source
        .deliver(delivery_request.clone())
        .await
        .map_err(ApplicationError::unavailable)?;
    awaken_credential_contract::verify_attempt_credential_delivery(
        &binding,
        &delivery_request.published_access,
        &delivery,
    )
    .map_err(|error| ApplicationError::internal(error.to_string()))?;
    // The signer is external to the Dispatch transaction. Re-read the exact
    // binding so a lease loss, cancellation or re-claim during issuance can
    // never return a usable carrier to the stale Worker.
    let current = service
        .dispatch
        .current_credential_binding(claim, fingerprint)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
    Ok(claim_material((current.as_ref() == Some(&binding)).then(
        || ClaimCredentialGrantMaterial::ProviderCredential(Box::new(delivery)),
    )))
}

async fn brokered_model(
    service: &WorkerDispatchService,
    authority: &ClaimAuthority,
    claim: &RunClaim,
    holder_is_caller: bool,
    request: Option<&RunDispatch>,
    fingerprint: CandidateFingerprint,
    operation: BrokeredGrantOperation,
) -> Result<Option<WorkerCredentialGrantMaterial>, ApplicationError> {
    let target = request
        .and_then(|request| frozen_candidate(request, &fingerprint))
        .and_then(|candidate| {
            awaken_credential_contract::brokered_grant_target(candidate, &claim.run_id.0).ok()
        });
    // Renew and close name an issued grant: it is held only by the claim and
    // candidate it was issued for.
    let issued_here = |grant_id: &str| {
        let mut ledger = service.brokered_grants.lock();
        prune_brokered_ledger(&mut ledger, authority.now_ms);
        ledger
            .get(grant_id)
            .is_some_and(|issued| &issued.claim == claim && issued.candidate == fingerprint)
    };
    let held = target.is_some()
        && match &operation {
            BrokeredGrantOperation::Acquire { .. } => true,
            BrokeredGrantOperation::Renew { grant_id }
            | BrokeredGrantOperation::Close { grant_id } => issued_here(grant_id),
        };
    let issuer = service.credential_grants.brokered.as_deref();
    let admission =
        credential_grant_admission(holder_is_caller, request.is_some(), held, issuer.is_some());
    let (Some(issuer), Some(target)) = (issuer, target) else {
        return refused(admission, CredentialGrantKind::BrokeredModel);
    };
    if admission != CredentialGrantAdmission::Issue {
        return refused(admission, CredentialGrantKind::BrokeredModel);
    }
    let issued = match operation {
        BrokeredGrantOperation::Close { grant_id } => {
            service.brokered_grants.lock().remove(&grant_id);
            return Ok(claim_material(Some(match issuer.close(&grant_id).await {
                Ok(()) => ClaimCredentialGrantMaterial::BrokeredModelClosed,
                Err(refusal) => ClaimCredentialGrantMaterial::BrokeredModelRefused(refusal),
            })));
        }
        BrokeredGrantOperation::Acquire { idempotency_key } => {
            issuer.acquire(target, idempotency_key).await
        }
        BrokeredGrantOperation::Renew { grant_id } => issuer.renew(&grant_id).await,
    };
    let access: BrokeredModelAccess = match issued {
        Ok(access) => access,
        Err(refusal) => {
            return Ok(claim_material(Some(
                ClaimCredentialGrantMaterial::BrokeredModelRefused(refusal),
            )));
        }
    };
    // The issue ledger is process memory: after a Coordinator restart it
    // refuses renew/close of grants issued before. That is safe only because
    // every grant this Coordinator hands out ends on the broker's own clock
    // within a bound; a grant the broker issued for longer is released at
    // once and never returned.
    if !brokered_grant_lifetime_is_bounded(access.grant_expires_at, authority.now_ms) {
        let _ = issuer.close(&access.grant_id).await;
        return Ok(claim_material(Some(
            ClaimCredentialGrantMaterial::BrokeredModelRefused(
                awaken_credential_contract::BrokeredGrantRefusal::TemporarilyUnavailable,
            ),
        )));
    }
    // Grant issuance is external I/O: a claim lost meanwhile receives nothing,
    // and the grant it would have used is released at once.
    let current = service
        .dispatch
        .claim_is_current(claim)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()));
    if !matches!(current, Ok(true)) {
        service.brokered_grants.lock().remove(&access.grant_id);
        let _ = issuer.close(&access.grant_id).await;
        return current.map(|_| None);
    }
    service.brokered_grants.lock().insert(
        access.grant_id.clone(),
        IssuedBrokeredGrant {
            claim: claim.clone(),
            candidate: fingerprint,
            expires_at_unix_s: access.grant_expires_at,
        },
    );
    Ok(claim_material(Some(
        ClaimCredentialGrantMaterial::BrokeredModel(access),
    )))
}

async fn mcp_generation_grant(
    service: &WorkerDispatchService,
    authority: &ClaimAuthority,
    generation: &McpGenerationRef,
    credential: &awaken_agent_contract::credential::CredentialRef,
    challenge: AuthChallenge,
) -> Result<Option<WorkerCredentialGrantMaterial>, ApplicationError> {
    let holder_is_caller = generation.runtime_incarnation() == authority.owner;
    let pinned = if holder_is_caller {
        Some(
            service
                .mcp_credential_grants
                .mcp_credential_grant(generation, credential)
                .await
                .map_err(mcp_grant_failure)?,
        )
    } else {
        None
    };
    let current = !matches!(
        pinned,
        None | Some(Err(McpCredentialGrantRefusal::StaleGeneration))
    );
    let issuer = service.credential_grants.mcp.as_deref();
    let admission = credential_grant_admission(
        holder_is_caller,
        current,
        matches!(pinned, Some(Ok(_))),
        issuer.is_some(),
    );
    let (Some(issuer), Some(Ok(access))) = (issuer, pinned) else {
        return refused(admission, CredentialGrantKind::McpBearer);
    };
    if admission != CredentialGrantAdmission::Issue {
        return refused(admission, CredentialGrantKind::McpBearer);
    }
    let refreshed = issuer
        .refresher(generation, &access)
        .refresh(&challenge)
        .await;
    // The refresh exchange is external I/O. A generation retired, reassigned
    // or expired meanwhile receives no bearer.
    let still_held = service
        .mcp_credential_grants
        .mcp_credential_grant(generation, credential)
        .await
        .map_err(mcp_grant_failure)?
        .is_ok();
    if !still_held {
        return Ok(None);
    }
    Ok(Some(WorkerCredentialGrantMaterial::McpGeneration(
        match refreshed {
            Some(Credential::Bearer(bearer)) => Some(GrantedSecret::new(bearer)),
            _ => None,
        },
    )))
}

fn mcp_grant_failure(
    failure: awaken_session_execution_contract::SessionRealizationControlFailure,
) -> ApplicationError {
    use awaken_session_execution_contract::SessionRealizationControlFailure as Failure;
    match failure {
        Failure::Invalid(message) => ApplicationError::invalid(message),
        Failure::Conflict => ApplicationError::conflict("Session MCP generation conflict"),
        other => ApplicationError::unavailable(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_returned_brokered_grant_always_ends_within_the_bound() {
        let now_ms = 1_000_000_000;
        let now_s = 1_000_000;
        for (rule, expiry, bounded) in [
            ("L2 past", now_s - 1, true),
            ("L2 minute", now_s + 60, true),
            ("L2 edge", now_s + MAX_BROKERED_GRANT_LIFETIME_S, true),
            (
                "L1 beyond",
                now_s + MAX_BROKERED_GRANT_LIFETIME_S + 1,
                false,
            ),
            ("L1 forever", i64::MAX, false),
        ] {
            assert_eq!(
                brokered_grant_lifetime_is_bounded(expiry, now_ms),
                bounded,
                "{rule}"
            );
        }
    }

    #[test]
    fn the_issue_ledger_forgets_a_grant_only_after_its_expiry_and_slack() {
        let claim = RunClaim {
            run_id: awaken_agent_contract::RunId("run".into()),
            owner: "worker/boot".into(),
            epoch: 1,
        };
        let issued = |expires_at_unix_s| IssuedBrokeredGrant {
            claim: claim.clone(),
            candidate: CandidateFingerprint("fp".into()),
            expires_at_unix_s,
        };
        let mut ledger = std::collections::BTreeMap::from([
            ("live".to_owned(), issued(100)),
            ("slack".to_owned(), issued(50)),
            ("gone".to_owned(), issued(10)),
        ]);
        prune_brokered_ledger(&mut ledger, 100_000);
        assert_eq!(
            ledger.keys().cloned().collect::<Vec<_>>(),
            ["live", "slack"],
            "expired beyond the slack only"
        );
    }
}
