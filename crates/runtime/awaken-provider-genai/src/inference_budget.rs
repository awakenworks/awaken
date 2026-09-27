//! Provider-side consumption of Runtime's one validated invocation budget.
use awaken_runtime_contract::llm::{
    DEFAULT_MODEL_ATTEMPT_WATCHDOG, DEFAULT_MODEL_RESPONSE_TIMEOUT, Error, ModelAttemptTiming,
    ModelInferenceBudgetError, Result,
};
use awaken_runtime_contract::model_operation::{ModelExecutionEvidence, NoDispatchProof};
use std::time::Duration;
use tokio::time::Instant;

/// All shipped provider dialects use this one pre-dispatch timing boundary.
/// Direct adapter calls have no outer watchdog; derive its unused allowance from
/// the default relation. Runtime calls validate the actual inherited watchdog.
pub(super) fn current_model_response_deadline(response: Duration) -> Result<Instant> {
    let timing = match awaken_runtime_contract::model_operation::current_model_attempt_timing() {
        Some(timing) => timing,
        None => {
            let allowance = DEFAULT_MODEL_ATTEMPT_WATCHDOG - DEFAULT_MODEL_RESPONSE_TIMEOUT;
            let watchdog = response
                .checked_add(allowance)
                .ok_or(ModelInferenceBudgetError)
                .map_err(ModelInferenceBudgetError::before_dispatch)?;
            ModelAttemptTiming::start_at(
                awaken_runtime_contract::llm::ModelInferenceClock::now(
                    &awaken_runtime_contract::llm::TokioModelInferenceClock,
                ),
                watchdog,
            )
            .map_err(ModelInferenceBudgetError::before_dispatch)?
        }
    };
    let deadline = timing
        .response_deadline(response)
        .map_err(ModelInferenceBudgetError::before_dispatch)?;
    if deadline
        <= awaken_runtime_contract::llm::ModelInferenceClock::now(
            &awaken_runtime_contract::llm::TokioModelInferenceClock,
        )
    {
        return Err(
            Error::Timeout("inference budget exhausted before provider dispatch".into())
                .with_execution_evidence(ModelExecutionEvidence::NotDispatched(
                    NoDispatchProof::request_projection(),
                )),
        );
    }
    Ok(deadline)
}
