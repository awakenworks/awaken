use serde::{Deserialize, Serialize};
use tirea_state::{GCounter, State};

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(
    path = "__kernel.stop_policy_runtime",
    action = "StopPolicyRuntimeAction",
    scope = "run"
)]
pub(super) struct StopPolicyRuntimeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default)]
    #[tirea(lattice)]
    pub total_input_tokens: GCounter,
    #[serde(default)]
    #[tirea(lattice)]
    pub total_output_tokens: GCounter,
}

/// Action type for `StopPolicyRuntimeState` reducer.
#[derive(Serialize, Deserialize)]
pub(crate) enum StopPolicyRuntimeAction {
    /// Record token usage from a single inference call.
    RecordTokens {
        started_at_ms: Option<u64>,
        prompt_tokens: usize,
        completion_tokens: usize,
    },
}

impl StopPolicyRuntimeState {
    pub(super) fn reduce(&mut self, action: StopPolicyRuntimeAction) {
        match action {
            StopPolicyRuntimeAction::RecordTokens {
                started_at_ms,
                prompt_tokens,
                completion_tokens,
            } => {
                if let Some(ms) = started_at_ms {
                    if self.started_at_ms.is_none() {
                        self.started_at_ms = Some(ms);
                    }
                }
                self.total_input_tokens.increment("_", prompt_tokens as u64);
                self.total_output_tokens
                    .increment("_", completion_tokens as u64);
            }
        }
    }
}
