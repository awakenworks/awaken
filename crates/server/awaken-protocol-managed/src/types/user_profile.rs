//! Wire types for the `user-profiles` resource (`beta.userProfiles.*`): a user
//! profile is the entity behind an agent run (an end user, a resold company, or
//! the platform itself). Pure serde shapes — the store, routes, and record→wire
//! projection live in `routes::user_profiles`.
//!
//! **This is a SEPARATE beta** from the rest of this crate: the official SDK
//! gates `beta.userProfiles.*` behind `anthropic-beta: user-profiles-2026-03-24`
//! (verified against `@anthropic-ai/sdk` v0.105.0 `resources/beta/user-profiles`),
//! NOT the `managed-agents-2026-04-01` header the module doc names for everything
//! else. `trust_grants` is the end-user's OAuth-style authorization grants — a
//! closed `{ status }` shape — and is deliberately NOT reused for GDPR consent
//! (ADR-0050 keeps consent on the neutral `DataSubject` aggregate).

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


/// `BetaUserProfile` — the wire projection.
#[derive(Debug, Clone, Serialize)]
pub struct UserProfile {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub metadata: BTreeMap<String, String>,
    pub relationship: Relationship,
    /// Trust grants keyed by grant name; empty on this single-machine surface.
    pub trust_grants: BTreeMap<String, serde_json::Value>,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `UserProfileCreateParams`.
#[derive(Debug, Clone, Deserialize)]
pub struct UserProfileCreateParams {
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub relationship: Relationship,
}

/// `BetaUserProfileEnrollmentURL` — the `POST .../enrollment_url` receipt: a
/// stable per-profile enrollment link with a validity horizon.
#[derive(Debug, Clone, Serialize)]
pub struct EnrollmentUrl {
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub url: String,
    pub expires_at: &'static str,
}

/// `UserProfileUpdateParams` — a partial update. `external_id` / `name` /
/// `relationship` replace when present; `metadata` is a merge where an **empty
/// string** value removes the key (the SDK's documented convention) and keys not
/// present are preserved.
#[derive(Debug, Clone, Deserialize)]
pub struct UserProfileUpdateParams {
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub relationship: Option<Relationship>,
}
