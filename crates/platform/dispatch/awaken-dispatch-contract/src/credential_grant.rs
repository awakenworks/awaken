// @formal evidence worker.secret_materialization_is_claim_fenced
//! The one grant-addressed credential endpoint of a registered Worker.
//!
//! A Worker asks for, or renews, the material of one credential grant it holds
//! under one exact lease: a Run grant (Provider credential delivery or brokered
//! model access) under its dispatch claim, an MCP bearer under the Session MCP
//! generation lease that staged it. Whatever the kind, the Coordinator applies
//! [`credential_grant_admission`]; the Worker only names the grant and never
//! supplies a holder, binding, route or credential selection.

use async_trait::async_trait;
use awaken_agent_contract::credential::CredentialRef;
use awaken_credential_contract::{
    ClaimCredentialGrant, ClaimCredentialGrantMaterial, GrantedSecret,
};
use awaken_session_configuration_contract::mcp::McpGenerationRef;
use awaken_worker_contract::{RunClaim, WorkerIdentity};
use serde::{Deserialize, Serialize};

use crate::DispatchError;

/// One credential grant together with the exact lease it is held under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "holding", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCredentialGrant {
    /// A grant frozen into the dispatch claim of one Run attempt.
    Claim {
        claim: RunClaim,
        grant: ClaimCredentialGrant,
    },
    /// The bearer pinned by one Session MCP generation, after the MCP server
    /// answered with an authentication challenge.
    McpGeneration {
        generation: McpGenerationRef,
        credential: CredentialRef,
        challenge_status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        www_authenticate: Option<String>,
    },
}

/// The issued material of one Worker credential grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "holding", content = "material", rename_all = "snake_case")]
pub enum WorkerCredentialGrantMaterial {
    Claim(ClaimCredentialGrantMaterial),
    /// `None`: the exact refresh or reload of the pinned bearer yielded no
    /// credential, so the Worker surfaces the original challenge.
    McpGeneration(Option<GrantedSecret>),
    /// The holding and grant are admitted, but this Coordinator installs no
    /// issuer for the kind (a split Coordinator without Control's Vault or
    /// delivery signer). A typed, terminal answer: no retry on this
    /// Coordinator can add the issuer. Brokered grants never take this path;
    /// a Coordinator without a broker answers `cloud_models_disabled`.
    IssuerMissing(CredentialGrantKind),
}

/// The kind of one Worker credential grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialGrantKind {
    ProviderCredential,
    BrokeredModel,
    McpBearer,
}

impl CredentialGrantKind {
    /// The classified code a Worker reports when this Coordinator installs
    /// no issuer for the kind.
    #[must_use]
    pub const fn issuer_missing_code(self) -> &'static str {
        match self {
            Self::ProviderCredential => "provider_credential_delivery_unavailable",
            Self::BrokeredModel => "cloud_models_disabled",
            Self::McpBearer => "mcp_credential_refresh_unavailable",
        }
    }
}

impl std::fmt::Display for CredentialGrantKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ProviderCredential => "Provider credential",
            Self::BrokeredModel => "brokered model",
            Self::McpBearer => "MCP bearer",
        })
    }
}

/// Request envelope of `POST /v1/worker/dispatch/credential_grant`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialGrantRequest {
    pub identity: WorkerIdentity,
    pub grant: WorkerCredentialGrant,
}

/// Worker-side transport of the one grant endpoint. `Ok(None)` means the
/// holding lease was lost while the grant was being issued: there is no
/// material for it and none was returned.
#[async_trait]
pub trait WorkerCredentialGrantTransport: Send + Sync {
    async fn credential_grant(
        &self,
        grant: WorkerCredentialGrant,
    ) -> Result<Option<WorkerCredentialGrantMaterial>, DispatchError>;
}

/// The Coordinator's decision for one grant request, before any issuer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialGrantAdmission {
    /// Issue the material, then re-read the holding before answering.
    Issue,
    /// The authenticated incarnation is not the holder named by the lease.
    ForeignHolder,
    /// The lease is not the owning aggregate's current, live lease.
    StaleHolding,
    /// The lease holds no such grant (unknown, revoked or another kind).
    GrantNotHeld,
    /// This Coordinator has no issuer for the grant kind: answered as the
    /// typed [`WorkerCredentialGrantMaterial::IssuerMissing`].
    IssuerMissing,
}

/// The single authorization rule of every Worker credential grant kind.
/// Callers derive each cause from the grant kind's owning aggregate; the
/// order is the precedence: identity, then holding, then grant, then issuer.
///
/// | Rule | holder is caller | holding current+live | grant held | issuer | effect |
/// |---|---|---|---|---|---|
/// | A1 | no | any | any | any | ForeignHolder |
/// | A2 | yes | no | any | any | StaleHolding |
/// | A3 | yes | yes | no | any | GrantNotHeld |
/// | A4 | yes | yes | yes | no | IssuerMissing |
/// | A5 | yes | yes | yes | yes | Issue |
#[must_use]
pub const fn credential_grant_admission(
    holder_is_caller: bool,
    holding_is_current: bool,
    grant_is_held: bool,
    issuer_installed: bool,
) -> CredentialGrantAdmission {
    if !holder_is_caller {
        CredentialGrantAdmission::ForeignHolder
    } else if !holding_is_current {
        CredentialGrantAdmission::StaleHolding
    } else if !grant_is_held {
        CredentialGrantAdmission::GrantNotHeld
    } else if !issuer_installed {
        CredentialGrantAdmission::IssuerMissing
    } else {
        CredentialGrantAdmission::Issue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grant_kind_is_decided_by_one_precedence_table() {
        // Expected column is written independently of the implementation:
        // the first failing cause in identity -> holding -> grant -> issuer
        // order decides, and only all four together issue material.
        use CredentialGrantAdmission::*;
        let mut rows = 0;
        for holder in [false, true] {
            for current in [false, true] {
                for held in [false, true] {
                    for issuer in [false, true] {
                        let expected = match (holder, current, held, issuer) {
                            (false, _, _, _) => ForeignHolder,
                            (true, false, _, _) => StaleHolding,
                            (true, true, false, _) => GrantNotHeld,
                            (true, true, true, false) => IssuerMissing,
                            (true, true, true, true) => Issue,
                        };
                        assert_eq!(
                            credential_grant_admission(holder, current, held, issuer),
                            expected,
                            "holder={holder} current={current} held={held} issuer={issuer}"
                        );
                        rows += 1;
                    }
                }
            }
        }
        assert_eq!(rows, 16);
    }

    #[test]
    fn the_wire_names_a_grant_by_its_lease_and_nothing_else() {
        let request = serde_json::json!({
            "identity": {"worker_id": "w", "incarnation_id": "i", "generation": 1},
            "grant": {
                "holding": "claim",
                "claim": {"run_id": "r", "owner": "w/i", "epoch": 3},
                "grant": {"kind": "provider_credential", "candidate_fingerprint": "fp"}
            }
        });
        let decoded: CredentialGrantRequest =
            serde_json::from_value(request.clone()).expect("exact grant request decodes");
        assert!(matches!(
            decoded.grant,
            WorkerCredentialGrant::Claim {
                grant: ClaimCredentialGrant::ProviderCredential { .. },
                ..
            }
        ));
        let mut widened = request;
        widened["grant"]["selected_plaintext_holder"] = serde_json::json!("other");
        assert!(serde_json::from_value::<CredentialGrantRequest>(widened).is_err());
    }
}
