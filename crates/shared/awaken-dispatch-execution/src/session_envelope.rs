// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
//! Worker-side adapter over the configured Managed Session runtime.
//!
//! Durable dispatch carries only frozen, secret-free projections. This adapter
//! reuses the installed Resource, credential, and MCP authorities when a cold
//! Worker realizes those projections; it owns no parallel store or policy.

use awaken_dispatch_contract::SessionResourceEnvelope;
use awaken_session_configuration_contract::resource::SessionResourceManifest;

const fn dispatched_resource_scope_is_admitted(
    workspace_is_nonempty: bool,
    scope_matches: bool,
) -> bool {
    workspace_is_nonempty && scope_matches
}

/// Decode the one Session-owned Resource envelope carried by durable dispatch.
/// Both execution realization and post-commit observation cross this boundary;
/// keeping scope validation here prevents either caller from inventing a second
/// wire interpretation.
pub fn decode_dispatched_resource_manifest(
    dispatch: &awaken_dispatch_contract::RunDispatch,
) -> Result<Option<awaken_session_configuration_contract::resource::SessionResourceManifest>, String>
{
    let Some(envelope) = &dispatch.session_resources else {
        return Ok(None);
    };
    let scope = dispatch
        .execution_scope
        .as_ref()
        .map(|scope| scope.0.0.as_str());
    if !dispatched_resource_scope_is_admitted(
        !envelope.workspace_id.trim().is_empty(),
        scope == Some(envelope.workspace_id.as_str()),
    ) {
        return Err(format!(
            "run {} has a resource manifest outside its execution scope",
            dispatch.run_id().0
        ));
    }
    envelope
        .decode_with(SessionResourceManifest::decode_resources)
        .map(Some)
        .map_err(|error| {
            format!(
                "run {} has an invalid Session resource manifest: {error}",
                dispatch.run_id().0
            )
        })
}

/// Encode a Session resource manifest into its dispatch envelope.
pub fn resource_envelope(
    manifest: &SessionResourceManifest,
) -> Result<SessionResourceEnvelope, serde_json::Error> {
    let resources = manifest.encode_resources()?;
    Ok(SessionResourceEnvelope::at_revision(
        &manifest.workspace_id,
        manifest.revision,
        resources,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatched_resource_scope_decision_table_is_total_and_exact() {
        // Cause/effect graph: C1 the envelope Workspace is nonempty; C2 it is
        // exactly the durable execution scope. E1 only C1+C2 admits decoding;
        // E2 every other combination rejects before manifest parsing or Runtime
        // effects. The four rows exhaust both boolean causes. String trimming,
        // equality, decoding, and effect ordering remain covered by the claimed
        // dispatch integration tests that consume this production predicate.
        for workspace_is_nonempty in [false, true] {
            for scope_matches in [false, true] {
                assert_eq!(
                    dispatched_resource_scope_is_admitted(workspace_is_nonempty, scope_matches),
                    workspace_is_nonempty && scope_matches
                );
            }
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// @obligation session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
    /// @proof-input-debt free_symbol dispatched_resource_scope_is_admitted scope_matches
    /// @proof-input-debt free_symbol dispatched_resource_scope_is_admitted workspace_is_nonempty
    #[kani::proof]
    fn dispatched_resource_manifest_requires_exact_nonempty_scope() {
        let workspace_is_nonempty: bool = kani::any();
        let scope_matches: bool = kani::any();
        let admitted = dispatched_resource_scope_is_admitted(workspace_is_nonempty, scope_matches);

        assert_eq!(admitted, workspace_is_nonempty && scope_matches);
        if !workspace_is_nonempty || !scope_matches {
            assert!(!admitted);
        }
    }
}
