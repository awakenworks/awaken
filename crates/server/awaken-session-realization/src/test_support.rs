// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-executor/src/host/session/environment_realization/fence_source_tests.rs
// @formal owner crates/server/awaken-session-executor/src/realization_composed_tests/session_realizations/execution_authority.rs
// @formal reason: Original lease and direct provider-boundary fixtures, reachable only through cfg(test) or explicit test-support. The provider entry returns the original private kernel Future; it introduces no execution grant, registry or alternate effect implementation.
//! Original lease observation fixtures, never execution authority.

/// Host fixtures use the actual Control-clock coordinate for live replies.
/// Deliberately rejects expired input: an expiry test must construct and age
/// its request observation explicitly, never silently manufacture fresh TTL.
pub fn test_lease_receipt(
    lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
) -> awaken_session_realization_contract::SessionRealizationLeaseReceipt {
    awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
        lease,
        runtime_unix_now_ms(),
    )
    .expect("live Control-clock fixture lease")
}

pub fn test_observed_lease(
    lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
) -> awaken_session_realization_contract::ObservedSessionRealizationLease {
    let started = awaken_authority_clock::authority_proof_started().unwrap();
    test_lease_receipt(lease).observed(started)
}

/// Deliberately aged evidence for negative Host tests. Keep the exact durable
/// fence bytes, but exhaust the local proof on the real platform clock before
/// returning it. This is never the fallback for a failed live fixture.
pub fn test_expired_observed_lease(
    lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
) -> awaken_session_realization_contract::ObservedSessionRealizationLease {
    let started = awaken_authority_clock::authority_proof_started().unwrap();
    let proof = awaken_session_realization_contract::SessionRealizationLeaseReceipt {
        lease,
        remaining_ms: std::num::NonZeroU64::new(1).unwrap(),
    }
    .observed(started);
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert!(
        proof
            .remaining_at(awaken_authority_clock::authority_proof_started().unwrap())
            .is_none()
    );
    proof
}

/// Direct fixture access to the original provider-boundary Future. W1-W7 in
/// Host's fence_source_tests exercise its real warmup/admission/recheck order;
/// this adds no poll layer, state, provider or execution grant.
pub fn create_session_environment<'a>(
    environments: &'a crate::environment_holding::EnvironmentHolding,
    provider: &'a crate::session_environment::SessionEnvironmentProvider,
    spec: &'a awaken_provisioning_contract::SandboxSpec,
    effect_fence: Option<&'a awaken_provisioning_contract::SandboxEffectFence>,
    authority: Option<&'a dyn awaken_provisioning_contract::CurrentExecutionAuthority>,
    source_handle: Option<&'a awaken_provisioning_contract::SandboxHandle>,
    intent: awaken_session_environment_contract::EnvironmentRealizationKind,
) -> impl std::future::Future<
    Output = Result<crate::session_environment::SessionEnvironment, crate::host_error::HostError>,
> + Send
+ 'a {
    environments.create_session_environment_with_provider_effect(
        provider,
        spec,
        effect_fence,
        authority,
        source_handle,
        intent,
    )
}

/// Fixture composition samples the canonical wall-clock port. Production
/// realization continues to read the Host-selected clock already injected.
pub fn runtime_unix_now_ms() -> u64 {
    use awaken_time::WallClockExt as _;
    let clock = awaken_time::system_clock();
    clock.now_ms()
}
