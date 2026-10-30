//! T33-C03 (cross-adapter consistency), ACP row: the shared injected upstream
//! fault (see `awaken-runtime/tests/cross_adapter_fault_local.rs`) projected by
//! the production ACP chain -- the raw transport error is classified by
//! `awaken_protocol_acp::classify_error` at the Prompt stage and becomes the
//! committed Run terminal through this crate's `failure_cause`.
//!
//! | fault | raw ACP error | Native authority (local row) | ACP today |
//! |---|---|---|---|
//! | F1 | `overloaded_error`, retry-after 7s | `Error(Inference{"overloaded"})`, retryable | `Error(Inference{"acp_failure"})` |
//! | Q1 | provider `session_limit`, retry-after 30s | `rate_limited` with its reset hint | `acp_failure`, hint only in prose |
//! | F2 | `server disconnected` mid-prompt (outcome unknown) | `Indeterminate` | `Error(Inference{"acp_failure"})` |
//! | R1 | refusal | `Stopped` | `Stopped` (preserved) |
//! | T1 | wall-clock deadline | `Stopped` | `Stopped` (preserved) |
//!
//! Oracles: O0 authority, uncertainty and retry hint are preserved; O1 no
//! adapter turns the fault into a successful end. O1 and the R1/T1 authorities
//! hold today; the O0 collapse is the reproducer below.
//! Negative control (executed): the same checker rejects a silent-success
//! mutant of the real ACP projection.

use awaken_agent_contract::agent::run::{EndCause, Failure};
use awaken_protocol_acp::{RawAcpError, Stage, classify_error, deadline_exceeded, refusal};

use crate::failure_cause;

const UPSTREAM_DETAIL: &str = "upstream 529 overloaded_error (retry-after 7s)";

/// The production ACP chain from a raw transport error to the Run terminal.
fn project(raw: &RawAcpError) -> EndCause {
    failure_cause(&classify_error(Stage::Prompt, raw))
}

fn f1_overloaded() -> RawAcpError {
    RawAcpError {
        message: UPSTREAM_DETAIL.to_string(),
        kind: Some("overloaded_error".to_string()),
        code: None,
        retry_after_secs: Some(7),
    }
}

fn q1_quota() -> RawAcpError {
    RawAcpError {
        message: "session_limit reached for this account".to_string(),
        kind: Some("session_limit".to_string()),
        code: None,
        retry_after_secs: Some(30),
    }
}

fn f2_unknown_outcome() -> RawAcpError {
    RawAcpError::message("server disconnected while the prompt was in flight")
}

/// O1: the projected terminal is never a successful end.
fn success_violation(row: &str, cause: &EndCause) -> Option<String> {
    matches!(cause, EndCause::NaturalEnd)
        .then(|| format!("O1 {row}: the fault surfaced as success: {cause:?}"))
}

#[test]
fn acp_projection_never_downgrades_an_upstream_fault_to_success() {
    for (row, raw) in [
        ("F1", f1_overloaded()),
        ("Q1", q1_quota()),
        ("F2", f2_unknown_outcome()),
    ] {
        let cause = project(&raw);
        assert_eq!(success_violation(row, &cause), None, "{row}");
        let EndCause::Error(failure) = &cause else {
            panic!("{row}: an upstream fault ends the Run in error, not {cause:?}");
        };
        assert!(
            failure.message().contains(&raw.message),
            "{row}: the provider's own account survives: {}",
            failure.message()
        );
    }
    // R1/T1: the authorities ACP does keep distinct from failure.
    assert_eq!(
        failure_cause(&refusal("policy")),
        EndCause::Stopped("agent refused".to_string()),
        "R1"
    );
    assert_eq!(
        failure_cause(&deadline_exceeded(60)),
        EndCause::Stopped("Step deadline exceeded".to_string()),
        "T1"
    );
}

#[test]
fn acp_success_checker_rejects_a_silent_success_mutant_of_the_real_projection() {
    let real = project(&f1_overloaded());
    assert_eq!(
        success_violation("F1", &real),
        None,
        "the real projection passes"
    );
    let mutant = EndCause::NaturalEnd;
    assert_ne!(mutant, real, "the mutant changes the projection");
    assert!(
        success_violation("F1 mutant", &mutant).is_some_and(|v| v.starts_with("O1")),
        "NC: a silently successful adapter is detected"
    );
}

#[test]
#[ignore = "product gap: ACP failure_cause collapses every classified failure to Inference{acp_failure}, losing the overloaded/rate_limited authority, the retry hint and the unknown-outcome uncertainty the Native path keeps"]
fn acp_projection_keeps_the_native_authority_uncertainty_and_retry_hint() {
    // F1: the same overloaded fault keeps the Native authority.
    match project(&f1_overloaded()) {
        EndCause::Error(Failure::Inference { code, .. }) => {
            assert_eq!(code, "overloaded", "F1 O0 authority");
        }
        other => panic!("F1 O0: expected the overloaded authority, got {other:?}"),
    }
    // Q1: a quota keeps the rate-limit authority and its reset hint.
    match project(&q1_quota()) {
        EndCause::Error(failure) => {
            assert_eq!(failure.code(), "rate_limited", "Q1 O0 authority");
            assert!(
                failure.message().contains("30"),
                "Q1 O0 the retry hint reaches the terminal: {}",
                failure.message()
            );
        }
        other => panic!("Q1 O0: expected the rate-limit authority, got {other:?}"),
    }
    // F2: an in-flight drop whose effect is unknown stays uncertain.
    assert_eq!(
        project(&f2_unknown_outcome()),
        EndCause::Indeterminate,
        "F2 O0 an unknown outcome is not a certain failure"
    );
}
