// @formal entrypoint remote_attempt.replay_uses_stable_message_identity
//! Stable, replay-safe identity for outbound A2A user messages.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::execution::{Error, Result};

const MESSAGE_ID_DOMAIN: &str = "a2a-message-v2";

#[must_use]
const fn message_identity_admitted(is_resume: bool, correlation_present: bool) -> bool {
    !is_resume || correlation_present
}

/// Derive one stable message id from Runtime-owned durable coordinates.
///
/// `None` identifies the Run's initial message. A resume is additionally bound
/// to its committed ticket correlation; structured fingerprinting prevents
/// delimiter-bearing opaque ids from aliasing another coordinate tuple.
pub(super) fn stable_message_id(
    run_id: &RunId,
    resume_correlation: Option<&str>,
) -> Result<String> {
    let correlation_present = resume_correlation.is_none_or(|value| !value.trim().is_empty());
    if !message_identity_admitted(resume_correlation.is_some(), correlation_present) {
        return Err(Error::Execution(
            "A2A resume message requires a committed correlation identity".to_string(),
        ));
    }
    let fingerprint = awaken_runtime_contract::resolution::content_fingerprint(&(
        MESSAGE_ID_DOMAIN,
        run_id.0.as_str(),
        resume_correlation,
    ))
    .expect("A2A message identity components always serialize");
    Ok(format!("{MESSAGE_ID_DOMAIN}:{fingerprint}"))
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation remote_attempt.replay_uses_stable_message_identity
    /// @proof-input-debt tautology message_identity_admitted
    #[kani::proof]
    fn a2a_message_identity_rejects_only_resume_without_committed_correlation() {
        // Causes: C1 initial versus resume message; C2 a non-empty committed
        // resume correlation is present. Effects: E1 admit the canonical
        // identity tuple; E2 reject before remote dispatch.
        //
        // | Rule | C1 resume | C2 correlation | Effect |
        // |---|---|---|---|
        // | R1 | no  | *   | E1 |
        // | R2 | yes | yes | E1 |
        // | R3 | yes | no  | E2 |
        let is_resume: bool = kani::any();
        let correlation_present: bool = kani::any();
        assert_eq!(
            message_identity_admitted(is_resume, correlation_present),
            !is_resume || correlation_present
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_phase_bound_and_delimiter_safe() {
        // Causes: C1 same Run/phase/ticket; C2 phase changes; C3 Run changes;
        // C4 ticket changes; C5 delimiter-bearing tuples that old concatenation
        // aliased; C6 empty resume correlation. Effects: E1 stable identity;
        // E2 distinct identity; E3 fail closed.
        //
        // | Rule | Identity relation | Effect |
        // |---|---|---|
        // | R1 | same Run/phase/ticket | E1 |
        // | R2 | phase, Run, or ticket differs | E2 |
        // | R3 | old delimiter-ambiguous tuples differ | E2 |
        // | R4 | resume correlation empty | E3 |
        let run_a = RunId("run-a".into());
        let initial = stable_message_id(&run_a, None).unwrap();
        assert_eq!(initial, stable_message_id(&run_a, None).unwrap(), "C1/E1");
        assert_ne!(
            initial,
            stable_message_id(&run_a, Some("ticket")).unwrap(),
            "C2/E2"
        );
        assert_ne!(
            initial,
            stable_message_id(&RunId("run-b".into()), None).unwrap(),
            "C3/E2"
        );
        assert_ne!(
            stable_message_id(&run_a, Some("ticket-a")).unwrap(),
            stable_message_id(&run_a, Some("ticket-b")).unwrap(),
            "C4/E2"
        );
        assert_ne!(
            stable_message_id(&RunId("run-a-b".into()), Some("c")).unwrap(),
            stable_message_id(&RunId("run-a".into()), Some("b-c")).unwrap(),
            "C5/E2"
        );
        assert!(stable_message_id(&run_a, Some("  ")).is_err(), "C6/E3");
    }
}
