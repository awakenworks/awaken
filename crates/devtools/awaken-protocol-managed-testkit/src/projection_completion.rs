//! Projection-install and terminal-cleanup completion every protocol test
//! Runtime shares, so no fake recreates a partial projection or a retired
//! cleanup path.

/// Protocol fixtures exercise application ordering, not OS suspend behavior.
/// Keep one explicit advancing test clock; unsupported production Runtimes
/// still fail closed instead of receiving a default timing capability.
pub fn observe_realization_request() -> Result<
    awaken_lease_contract::authority::AuthorityProofStarted,
    awaken_runtime_contract::runs::RunError,
> {
    static ORIGIN: std::sync::OnceLock<awaken_time::mono::Instant> = std::sync::OnceLock::new();
    let origin = *ORIGIN.get_or_init(awaken_time::mono::now);
    let now = awaken_time::mono::now();
    Ok(
        awaken_lease_contract::authority::AuthorityProofStarted::from_clock_sample(
            now,
            now.duration_since(origin),
        ),
    )
}

/// Validate the complete Session projection accepted by protocol test doubles
/// and return the legacy preparation view only when the selected installation
/// mode performs preparation. Keeping this rule here prevents each fake from
/// recreating a partial projection or silently accepting empty coordinates.
pub fn complete_session_projection_init(
    thread: &str,
    projection: &awaken_session_execution_contract::FrozenSessionProjection,
    mode: &awaken_session_execution_contract::SessionProjectionInstallMode,
) -> Result<
    Option<awaken_session_configuration_contract::SessionInit>,
    awaken_runtime_contract::runs::RunError,
> {
    if thread.is_empty()
        || projection.workspace_id.is_empty()
        || projection.baseline.agent_id.is_empty()
    {
        return Err(awaken_runtime_contract::runs::RunError::bad_request(
            "test Session projection must carry complete frozen coordinates",
        ));
    }
    Ok(mode.prepares_session().then(|| projection.session_init()))
}

/// Complete the preparation half of a protocol test Runtime through the same
/// closed authorization join as production. Keeping this in the shared test
/// adapter prevents each router/state fixture from recreating the retired
/// one-stage cleanup path or silently ignoring the effect echoed by Control.
pub fn complete_terminal_cleanup_preparation(
    effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    authorization: &awaken_session_execution_contract::SessionTerminalCleanupPreparationAuthorization,
) -> Result<
    awaken_session_contract::SessionCleanupPreparation,
    awaken_runtime_contract::runs::RunError,
> {
    authorization
        .verify_for(effect)
        .map_err(|error| awaken_runtime_contract::runs::RunError::internal(error.to_string()))?;
    let provider_prepared_effect_fence = effect
        .sandbox_effect_fence()
        .map_err(|error| awaken_runtime_contract::runs::RunError::internal(error.to_string()))?;
    awaken_session_contract::SessionCleanupPreparation::try_new(
        effect,
        provider_prepared_effect_fence,
        Vec::new(),
    )
    .map_err(|error| awaken_runtime_contract::runs::RunError::internal(error.to_string()))
}

/// Complete the physical half of a protocol test Runtime from the one typed
/// aggregate command. The receipt constructor remains the canonical identity
/// owner; this helper owns no cleanup state or compatibility behavior.
#[must_use]
pub fn complete_terminal_cleanup_disposal(
    effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
) -> awaken_session_contract::SessionCleanupDisposalReceipt {
    awaken_session_contract::SessionCleanupDisposalReceipt::new(&effect.command)
}
