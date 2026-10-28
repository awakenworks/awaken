// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Closed request DTOs only; credential access and effects stay in the existing route handlers.
//! Credential/profile request wire shapes; no authorization or materialization.

/// A cooldown signal an operator (or an external rate-limit integration) records
/// against a credential source. `kind` maps to a failure
/// [`Disposition`](awaken_agent_contract::resilience::Disposition): `quota` cools
/// until `retry_after_secs` (or a default window); `exhausted` cools until cleared;
/// `available` / `clear` lifts any cooldown; `transient` / `permanent` are no-ops on
/// availability (they are retry/next-binding decisions, not identity cooldowns).
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CooldownRequest {
    pub(super) kind: String,
    #[serde(default)]
    pub(super) retry_after_secs: Option<u64>,
}

/// Resolve an authored profile within a workspace's credential scope.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResolveProfileRequest {
    pub(super) workspace_id: String,
}

/// Live-validate a credential against a model's resolved provider endpoint.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ValidateCredentialRequest {
    pub(super) workspace_id: String,
    #[serde(default)]
    pub(super) model_id: Option<String>,
    /// Generic hosted-governance validation mode. The durable source id is the
    /// path identity, so this field is mutually exclusive with `model_id`.
    #[serde(default)]
    pub(super) provider_ref: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListCredentialsQuery {
    #[serde(default)]
    pub(super) workspace_id: String,
    #[serde(default)]
    pub(super) provider_ref: Option<String>,
    #[serde(default)]
    pub(super) idempotency_key: Option<String>,
    #[serde(default)]
    pub(super) hosted_only: bool,
}
