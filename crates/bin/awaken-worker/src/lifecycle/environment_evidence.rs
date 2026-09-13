//! One conservative decision boundary for Worker-local Environment evidence.

use std::time::Duration;

use awaken_runtime_contract::authority_lease::AuthorityLeaseTiming;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeObservation {
    Due,
    Proven,
    Lost,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvidenceAction {
    StartProbe,
    RefreshFromRequestStart,
    RetryWithinCurrentProof,
    CloseExpired,
    CloseLost,
}

const fn evidence_action(
    proof_age_nanos: u128,
    proof_window_nanos: u128,
    observation: ProbeObservation,
) -> EvidenceAction {
    if proof_age_nanos >= proof_window_nanos {
        return EvidenceAction::CloseExpired;
    }
    match observation {
        ProbeObservation::Due => EvidenceAction::StartProbe,
        ProbeObservation::Proven => EvidenceAction::RefreshFromRequestStart,
        ProbeObservation::Lost => EvidenceAction::CloseLost,
        ProbeObservation::Unknown => EvidenceAction::RetryWithinCurrentProof,
    }
}

pub(crate) fn proof_is_fresh(elapsed: Duration, proof_window: Duration) -> bool {
    evidence_action(
        elapsed.as_nanos(),
        proof_window.as_nanos(),
        ProbeObservation::Due,
    ) == EvidenceAction::StartProbe
}

/// A timeout means unknown evidence, not invalid evidence and not fresh
/// evidence. Retry only inside the last successfully proven, finite window.
/// Provider errors are deliberately fail-closed: the current provider contract
/// does not distinguish transport errors from a violated security policy.
pub(super) async fn monitor<Probe, ProbeFuture, Error>(
    mut last_proof: tokio::time::Instant,
    timing: AuthorityLeaseTiming,
    mut probe: Probe,
) where
    Probe: FnMut() -> ProbeFuture,
    ProbeFuture: std::future::Future<Output = Result<(), Error>>,
    Error: std::fmt::Display,
{
    let mut next_attempt = last_proof + timing.renew_interval();
    loop {
        let deadline = last_proof + timing.proof_window();
        tokio::time::sleep_until(next_attempt.min(deadline)).await;
        let started = tokio::time::Instant::now();
        if !proof_is_fresh(
            started.saturating_duration_since(last_proof),
            timing.proof_window(),
        ) {
            awaken_observability::record_worker_authority_loss("environment_evidence_expired");
            tracing::error!(
                proof_age_ms = started.saturating_duration_since(last_proof).as_millis(),
                proof_window_ms = timing.proof_window().as_millis(),
                "worker sandbox evidence expired; draining locally"
            );
            return;
        }
        let result =
            tokio::time::timeout_at((started + timing.request_timeout()).min(deadline), probe())
                .await;
        let finished = tokio::time::Instant::now();
        let elapsed = finished.saturating_duration_since(last_proof);
        let probe_elapsed_ms = finished.saturating_duration_since(started).as_millis();
        let proof_age_ms = elapsed.as_millis();
        let remaining_proof_ms = timing.remaining_proof_after(elapsed).as_millis();
        let observation = match &result {
            Ok(Ok(())) => ProbeObservation::Proven,
            Ok(Err(_)) => ProbeObservation::Lost,
            Err(_) => ProbeObservation::Unknown,
        };
        match evidence_action(
            elapsed.as_nanos(),
            timing.proof_window().as_nanos(),
            observation,
        ) {
            EvidenceAction::RefreshFromRequestStart => {
                // Request-start anchoring prevents response latency from minting
                // additional local authority.
                last_proof = started;
                next_attempt = started + timing.renew_interval();
            }
            EvidenceAction::CloseLost => {
                let Ok(Err(error)) = result else {
                    unreachable!("CloseLost is derived only from a provider rejection")
                };
                awaken_observability::record_worker_authority_loss("environment_evidence_lost");
                tracing::error!(error = %error, probe_elapsed_ms, proof_age_ms,
                    remaining_proof_ms, "worker sandbox evidence no longer holds; draining locally");
                return;
            }
            EvidenceAction::RetryWithinCurrentProof => {
                tracing::warn!(
                    probe_elapsed_ms,
                    proof_age_ms,
                    remaining_proof_ms,
                    request_budget_ms = timing.request_timeout().as_millis(),
                    "worker sandbox evidence probe timed out; retrying within existing proof"
                );
                // The timed-out probe already consumed the retry pacing budget.
                // Sleeping again can skip the remaining proof window entirely
                // under scheduler load, turning one unknown Kubernetes read
                // into irreversible Worker authority loss. Retry immediately;
                // timeout_at still caps this attempt at the original deadline
                // and no successful response can extend that deadline.
                next_attempt = tokio::time::Instant::now();
            }
            EvidenceAction::CloseExpired => {
                awaken_observability::record_worker_authority_loss("environment_evidence_expired");
                tracing::error!(
                    probe_elapsed_ms,
                    proof_age_ms,
                    remaining_proof_ms,
                    request_budget_ms = timing.request_timeout().as_millis(),
                    "worker sandbox evidence expired during probe; draining locally"
                );
                return;
            }
            EvidenceAction::StartProbe => {
                unreachable!("post-probe observations cannot request another probe directly")
            }
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn environment_evidence_decision_never_refreshes_expired_proof() {
    // Causes: C1 proof age is before/at/after its window; C2 observation is
    // due/proven/lost/unknown. Effects: E1 start, E2 refresh from request start,
    // E3 retry without refresh, E4 close expired, E5 close lost. Decision table:
    // expired x any -> E4; fresh x due/proven/lost/unknown -> E1/E2/E5/E3.
    let proof_age_nanos: u128 = kani::any();
    let proof_window_nanos: u128 = kani::any();
    let observation = match kani::any::<u8>() % 4 {
        0 => ProbeObservation::Due,
        1 => ProbeObservation::Proven,
        2 => ProbeObservation::Lost,
        _ => ProbeObservation::Unknown,
    };
    let action = evidence_action(proof_age_nanos, proof_window_nanos, observation);
    assert_eq!(
        action == EvidenceAction::CloseExpired,
        proof_age_nanos >= proof_window_nanos
    );
    assert_eq!(
        action == EvidenceAction::RefreshFromRequestStart,
        proof_age_nanos < proof_window_nanos && observation == ProbeObservation::Proven
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_action_covers_every_window_and_observation_class() {
        // Causes and effects are the same C1/C2 and E1-E5 table documented on
        // the Kani harness. Boundary ages 9/10/11 cover before/equal/after;
        // every observation covers all four fresh rows and the expired collapse.
        let observations = [
            (ProbeObservation::Due, EvidenceAction::StartProbe),
            (
                ProbeObservation::Proven,
                EvidenceAction::RefreshFromRequestStart,
            ),
            (ProbeObservation::Lost, EvidenceAction::CloseLost),
            (
                ProbeObservation::Unknown,
                EvidenceAction::RetryWithinCurrentProof,
            ),
        ];
        for (observation, fresh_action) in observations {
            assert_eq!(evidence_action(9, 10, observation), fresh_action);
            assert_eq!(
                evidence_action(10, 10, observation),
                EvidenceAction::CloseExpired
            );
            assert_eq!(
                evidence_action(11, 10, observation),
                EvidenceAction::CloseExpired
            );
        }
    }
}
