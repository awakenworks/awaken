use super::*;
use crate::model_operation::{
    ModelExecutionEvidence, NoDispatchProof, ResponseEnvelope, TerminalReceipt,
};
use crate::resilience::{Classify, Disposition};

fn refusal(kind: usize) -> Error {
    match kind {
        0 => Error::Provider("admission dependency".into()),
        1 => Error::RateLimited {
            message: "admission pressure".into(),
            retry_after: Some(Duration::from_secs(2)),
        },
        2 => Error::Overloaded {
            message: "admission overloaded".into(),
            retry_after: Some(Duration::from_secs(3)),
        },
        3 => Error::UsageLimit {
            message: "admission funding".into(),
            reset_after: Some(Duration::from_secs(4)),
        },
        4 => Error::Unauthorized("admission policy".into()),
        _ => Error::Cancelled("admission cancelled".into()),
    }
}

#[test]
fn admission_grade_and_physical_evidence_remain_independent() {
    // Cause/effect table: six grades x three original physical observations x
    // two wrapper orders. Origin changes only the durable layer-qualified code;
    // grade, retryability, delay/reset hints and physical evidence are retained.
    // In particular an Admission label cannot upgrade Unknown to a proof.
    let codes = [
        "model_admission_unavailable",
        "model_admission_rate_limited",
        "model_admission_unavailable",
        "model_admission_usage_limit",
        "model_admission_rejected",
        "cancelled",
    ];
    for (kind, code) in codes.into_iter().enumerate() {
        for evidence in [
            ModelExecutionEvidence::Unknown,
            ModelExecutionEvidence::NotDispatched(NoDispatchProof::executor_resolution()),
            ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
                ResponseEnvelope::CompleteErrorBody,
            )),
        ] {
            for origin_first in [false, true] {
                let source = refusal(kind);
                let grade = source.class();
                let hints = (source.retry_after(), source.reset_hint());
                let error = if origin_first {
                    Error::Admission {
                        source: Box::new(source),
                    }
                    .with_execution_evidence(evidence)
                } else {
                    Error::Admission {
                        source: Box::new(source.with_execution_evidence(evidence)),
                    }
                };
                assert_eq!(error.class(), grade);
                assert_eq!(error.is_retryable(), grade.is_retryable());
                assert_eq!((error.retry_after(), error.reset_hint()), hints);
                assert_eq!(error.execution_evidence(), evidence);
                assert_eq!(
                    error.code(),
                    code,
                    "grade {kind}, origin_first {origin_first}"
                );
            }
        }
    }
}

#[test]
fn admission_refusals_cannot_select_another_provider_or_credential() {
    // R1 transient/throttle/overload/hard-quota admission refusal -> no
    // Provider routing action, while the grade still owns its ordinary retry
    // budget. R2 ordinary Provider errors retain the original routing result.
    // Physical observations cannot change either origin's routing rule.
    for kind in 0..4 {
        let provider = refusal(kind);
        assert_eq!(provider.disposition(), provider.class().disposition());
        let error = Error::Admission {
            source: Box::new(provider),
        }
        .with_execution_evidence(ModelExecutionEvidence::NotDispatched(
            NoDispatchProof::executor_resolution(),
        ));
        assert_eq!(
            error.disposition(),
            Disposition::StopRouting,
            "grade {kind}"
        );
        assert!(!error.disposition().prefers_failover(), "grade {kind}");
    }
}
