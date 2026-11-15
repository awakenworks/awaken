// @formal evidence worker.secret_materialization_is_claim_fenced
//! Credential grants an attempt holds under its exact dispatch claim.
//!
//! A registered Worker names one grant it holds; the Coordinator that froze the
//! grant decides it and issues its current material. The Worker never supplies
//! the holder, binding, provider route or credential selection, so a grant
//! request cannot widen what the claim already authorized. Provider material
//! remains a recipient-sealed [`AttemptCredentialDelivery`]; a brokered model
//! grant carries the short-lived Gateway capability the broker issued for the
//! exact published candidate.

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use awaken_agent_contract::credential::CandidateFingerprint;
use serde::{Deserialize, Serialize};

use crate::AttemptCredentialDelivery;

/// A short-lived secret value addressed to the exact holder of one grant. It
/// crosses only the authenticated Worker transport and is converted to a
/// [`RedactedString`] on receipt; it is never part of durable state and its
/// `Debug` form never shows the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantedSecret(String);

impl GrantedSecret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn from_redacted(value: &RedactedString) -> Self {
        Self(value.expose_secret().to_owned())
    }

    #[must_use]
    pub fn into_redacted(self) -> RedactedString {
        RedactedString::new(self.0)
    }
}

impl std::fmt::Debug for GrantedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantedSecret(<redacted>)")
    }
}

/// What the holder of a brokered model grant asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrokeredGrantOperation {
    /// Acquire a new grant for one model operation. The key is derived from
    /// the Runtime-owned model operation identity, never from model input.
    Acquire { idempotency_key: String },
    /// Renew a grant issued for the same candidate.
    Renew { grant_id: String },
    /// Best-effort release after one native request.
    Close { grant_id: String },
}

/// One credential grant held under a dispatch claim, addressed only by the
/// frozen candidate it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClaimCredentialGrant {
    /// The current recipient-sealed delivery of the Provider credential frozen
    /// into the claim's attempt binding for this candidate.
    ProviderCredential {
        candidate_fingerprint: CandidateFingerprint,
    },
    /// Brokered access for this exact brokered Provider candidate.
    BrokeredModel {
        candidate_fingerprint: CandidateFingerprint,
        operation: BrokeredGrantOperation,
    },
}

/// The short-lived access a broker issued for one exact candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokeredModelAccess {
    pub grant_id: String,
    pub gateway_base_url: String,
    pub capability: GrantedSecret,
    pub grant_expires_at: i64,
}

/// The broker's own refusal of a brokered grant. The claim was authorized;
/// these are entitlement, quota and availability answers that the Worker maps
/// onto its model error taxonomy unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum BrokeredGrantRefusal {
    AuthenticationRequired,
    AccountSelectionRequired,
    SubscriptionRequired,
    ModelNotEntitled,
    ModelUnavailable,
    InsufficientBalance,
    QuotaExceeded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    RateLimited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    TemporarilyUnavailable,
    InvalidRequest,
    /// Brokered model supply is disabled for this Coordinator.
    CloudModelsDisabled,
    /// The Coordinator holds no authenticated broker identity.
    SignInRequired,
}

/// The issued material of one claim grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "material", rename_all = "snake_case")]
pub enum ClaimCredentialGrantMaterial {
    ProviderCredential(Box<AttemptCredentialDelivery>),
    BrokeredModel(BrokeredModelAccess),
    BrokeredModelClosed,
    BrokeredModelRefused(BrokeredGrantRefusal),
}

/// The attempt's claim-bound grant port. `Ok(None)` means the claim was
/// superseded while the grant was being issued: no material exists for it.
#[async_trait]
pub trait ClaimCredentialGrants: Send + Sync {
    async fn grant(
        &self,
        grant: ClaimCredentialGrant,
    ) -> Result<Option<ClaimCredentialGrantMaterial>, String>;
}

/// The exact public target of one brokered candidate, derived by the
/// Coordinator from the frozen candidate under the claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokeredGrantTarget {
    pub provider: String,
    pub original_model_id: String,
    pub native_protocol: String,
    pub local_run_correlation: String,
}

/// The broker's public routing target of one brokered candidate: its provider
/// routing label, original model id and native protocol. The single
/// derivation shared by the Worker executor and the Coordinator issuer.
///
/// | Rule | provider label | model | dialect | effect |
/// |---|---|---|---|---|
/// | T1 | `vendor@N` or `vendor` | nonempty | anthropic/openai chat/responses | exact target |
/// | T2 | any | any | another dialect | refused: unsupported dialect |
/// | T3 | empty after the version | any | supported | refused: incomplete target |
/// | T4 | any | blank | supported | refused: incomplete target |
pub fn brokered_target_parts(
    provider_ref: &str,
    model: &str,
    api_dialect: &str,
) -> Result<(String, String, String), String> {
    // Brokered supply names a provider routing label, not a Vault source
    // authorization coordinate; the catalog may give a versioned or bare label.
    let provider = provider_ref
        .split_once('@')
        .map_or(provider_ref, |(provider, _)| provider)
        .trim();
    let native_protocol = match api_dialect {
        "anthropic_messages" => "anthropic_messages",
        "open_ai_chat" => "openai_chat_completions",
        "open_ai_responses" => "openai_responses",
        _ => return Err(format!("unsupported brokered API dialect `{api_dialect}`")),
    };
    if provider.is_empty() || model.trim().is_empty() {
        return Err("brokered publication has an incomplete public target".into());
    }
    Ok((provider.into(), model.into(), native_protocol.into()))
}

/// The broker target of one frozen brokered candidate under the claim of
/// `run_id`. Refuses every candidate that is not a brokered Provider route.
pub fn brokered_grant_target(
    candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
    run_id: &str,
) -> Result<BrokeredGrantTarget, String> {
    let awaken_runtime_contract::resolved::ModelProvisioning::Provider {
        provider_ref,
        access_kind: awaken_runtime_contract::resolved::ProviderAccessKind::Brokered,
        endpoint,
        ..
    } = candidate.provisioning()
    else {
        return Err("candidate is not a brokered Provider route".into());
    };
    let (provider, original_model_id, native_protocol) = brokered_target_parts(
        provider_ref,
        candidate.binding().identity().1,
        &endpoint.api_dialect,
    )?;
    Ok(BrokeredGrantTarget {
        provider,
        original_model_id,
        native_protocol,
        local_run_correlation: run_id.to_owned(),
    })
}

/// Coordinator-side issuer of brokered grants: the deployment's authenticated
/// broker identity. Refusals are business answers, not transport failures.
#[async_trait]
pub trait BrokeredGrantIssuer: Send + Sync {
    async fn acquire(
        &self,
        target: BrokeredGrantTarget,
        idempotency_key: String,
    ) -> Result<BrokeredModelAccess, BrokeredGrantRefusal>;

    async fn renew(&self, grant_id: &str) -> Result<BrokeredModelAccess, BrokeredGrantRefusal>;

    async fn close(&self, grant_id: &str) -> Result<(), BrokeredGrantRefusal>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granted_secret_never_prints_and_round_trips_only_on_the_wire() {
        let secret = GrantedSecret::new("capability-value");
        assert_eq!(format!("{secret:?}"), "GrantedSecret(<redacted>)");
        let access = BrokeredModelAccess {
            grant_id: "g".into(),
            gateway_base_url: "https://gateway.invalid".into(),
            capability: secret.clone(),
            grant_expires_at: 1,
        };
        assert!(!format!("{access:?}").contains("capability-value"));
        let wire = serde_json::to_value(ClaimCredentialGrantMaterial::BrokeredModel(access))
            .expect("serialize");
        assert_eq!(wire["material"]["capability"], "capability-value");
        let back: ClaimCredentialGrantMaterial = serde_json::from_value(wire).expect("decode");
        let ClaimCredentialGrantMaterial::BrokeredModel(access) = back else {
            panic!("kind survives the wire");
        };
        assert_eq!(
            access.capability.into_redacted().expose_secret(),
            "capability-value"
        );
    }

    #[test]
    fn grant_requests_reject_unknown_fields_and_kinds() {
        let exact = serde_json::json!({
            "kind": "brokered_model",
            "candidate_fingerprint": "fp",
            "operation": {"op": "renew", "grant_id": "g"}
        });
        assert!(serde_json::from_value::<ClaimCredentialGrant>(exact.clone()).is_ok());
        let mut widened = exact.clone();
        widened["selected_plaintext_holder"] = serde_json::json!("other");
        assert!(serde_json::from_value::<ClaimCredentialGrant>(widened).is_err());
        let mut unknown = exact;
        unknown["kind"] = serde_json::json!("vault_secret");
        assert!(serde_json::from_value::<ClaimCredentialGrant>(unknown).is_err());
    }
}
