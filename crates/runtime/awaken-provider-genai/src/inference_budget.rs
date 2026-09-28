//! Provider-side reading of the one caller-declared inference budget.
//!
//! Adapters own no timeout of their own. Under Runtime they read the attempt
//! timing Runtime derived from its caller's budget; a direct adapter call
//! (no Runtime around it) runs one attempt of the default budget.
use awaken_runtime_contract::llm::{
    Error, ModelAttemptTiming, ModelInferenceBudget, ModelInferenceClock, Result,
    TokioModelInferenceClock,
};
use awaken_runtime_contract::model_operation::{ModelExecutionEvidence, NoDispatchProof};

/// The inherited timing of this attempt, refused before dispatch when its
/// response deadline is already spent.
pub(super) fn current_attempt_timing() -> Result<ModelAttemptTiming> {
    let now = TokioModelInferenceClock.now();
    let timing = match awaken_runtime_contract::model_operation::current_model_attempt_timing() {
        Some(timing) => timing,
        None => ModelAttemptTiming::standalone(now, ModelInferenceBudget::DEFAULT)
            .map_err(awaken_runtime_contract::llm::ModelInferenceBudgetError::before_dispatch)?,
    };
    if timing.response_deadline() <= TokioModelInferenceClock.now() {
        return Err(
            Error::Timeout("inference budget exhausted before provider dispatch".into())
                .with_execution_evidence(ModelExecutionEvidence::NotDispatched(
                    NoDispatchProof::request_projection(),
                )),
        );
    }
    Ok(timing)
}
