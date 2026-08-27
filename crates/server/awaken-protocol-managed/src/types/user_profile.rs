//! Wire types for the `user-profiles` resource (`beta.userProfiles.*`): a user
//! profile is the entity behind an agent run (an end user, a resold company, or
//! the platform itself). Pure serde shapes — the store, routes, and record→wire
//! projection live in `routes::user_profiles`.
//!
//! **This is a SEPARATE beta** from the rest of this crate: the official SDK
//! gates `beta.userProfiles.*` behind its own version: SDK 0.117.1 emits
//! `user-profiles-2026-03-24`, while SDK 0.120.0 and the current oracle emit
//! `user-profiles-2026-08-18`. Neither uses the `managed-agents-2026-04-01`
//! header. `trust_grants` is the end-user's OAuth-style authorization grants —
//! a closed `{ status }` shape — and is deliberately NOT reused for GDPR
//! consent (ADR-0050 keeps consent on the neutral `DataSubject` aggregate).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How the entity behind a profile relates to the API-key owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum Relationship {
    #[default]
    External,
    Resold,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessType {
    Application,
    Passthrough,
}

/// `BetaUserProfile` — the wire projection.
#[derive(Debug, Clone, Serialize)]
pub struct UserProfile {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub metadata: BTreeMap<String, String>,
    pub relationship: Relationship,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_type: Option<AccessType>,
    /// Trust grants keyed by grant name; empty on this single-machine surface.
    pub trust_grants: BTreeMap<String, TrustGrant>,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrustGrant {
    pub status: TrustGrantStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustGrantStatus {
    Active,
    Pending,
    Rejected,
}

/// `UserProfileCreateParams`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserProfileCreateParams {
    #[serde(default, deserialize_with = "super::presence::optional_non_null")]
    pub access_type: Option<AccessType>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "super::presence::optional_non_null")]
    pub relationship: Option<Relationship>,
}

/// `BetaUserProfileEnrollmentURL` — the `POST .../enrollment_url` receipt: a
/// stable per-profile enrollment link with a validity horizon.
#[derive(Debug, Clone, Serialize)]
pub struct EnrollmentUrl {
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub url: String,
    pub expires_at: String,
}

/// `UserProfileUpdateParams` — a partial update. `external_id` / `name` /
/// `relationship` replace when present; `metadata` is a merge where an **empty
/// string** value removes the key (the SDK's documented convention) and keys not
/// present are preserved.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserProfileUpdateParams {
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub access_type: Option<Option<AccessType>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub external_id: Option<Option<String>>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub name: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub relationship: Option<Option<Relationship>>,
    /// Authorization-style trust grants. These are not GDPR consent grants.
    #[serde(default)]
    pub trust_grants: Option<BTreeMap<String, TrustGrant>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_and_relationship_presence_matches_create_and_update_contracts() {
        // Causes: the fixtures below establish `access and relationship presence` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `matches create and update contracts` and every asserted
        // state transition or side effect must hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: create uses optional-but-non-null vocabulary;
        // update uses three-state fields so null can clear legacy/access state.
        //
        // Decision table:
        // | Rule | operation | JSON field | decoded effect |
        // | P1 | create | omitted | None |
        // | P2 | create | null | reject before route/application |
        // | P3 | update | omitted | outer None (preserve) |
        // | P4 | update | null | Some(None) (clear) |
        let created: UserProfileCreateParams =
            serde_json::from_value(serde_json::json!({})).expect("P1 omission");
        assert_eq!(created.access_type, None, "P1");
        assert_eq!(created.relationship, None, "P1");
        assert!(
            serde_json::from_value::<UserProfileCreateParams>(
                serde_json::json!({ "access_type": null })
            )
            .is_err(),
            "P2"
        );
        assert!(
            serde_json::from_value::<UserProfileCreateParams>(
                serde_json::json!({ "relationship": null })
            )
            .is_err(),
            "P2"
        );
        let omitted: UserProfileUpdateParams =
            serde_json::from_value(serde_json::json!({})).expect("P3 omission");
        assert_eq!(omitted.access_type, None, "P3");
        assert_eq!(omitted.relationship, None, "P3");
        let cleared: UserProfileUpdateParams = serde_json::from_value(serde_json::json!({
            "access_type": null,
            "relationship": null,
        }))
        .expect("P4 null");
        assert_eq!(cleared.access_type, Some(None), "P4");
        assert_eq!(cleared.relationship, Some(None), "P4");
    }
}
