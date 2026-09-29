//! Message identity for the result of a call owned by one durable ToolBatch.
//! The aggregate remains the only source of the Run/Step coordinate and call
//! membership; this module adds no result state or publication path.

use super::*;

impl ToolBatch {
    /// Message identity for a result staged by this durable batch. Execution
    /// and recovery reuse the same coordinate even when an assistant Step id
    /// advanced independently during resume.
    pub fn result_message_id(
        &self,
        call_id: &str,
    ) -> Result<awaken_agent_contract::agent::message::Id, ToolBatchError> {
        if !self.calls.iter().any(|entry| entry.call.call_id == call_id) {
            return Err(ToolBatchError::UnknownCall(call_id.to_string()));
        }
        Ok(
            awaken_agent_contract::agent::message::Id::tool_result_for_batch(
                self.id.as_str(),
                call_id,
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_message_id_uses_the_durable_batch_coordinate_after_recovery() {
        // Causes: C1 same Run/Step/Call before or after serialization; C2 same
        // provider Call in another Step or Run; C3 another Call in the batch;
        // C4 absent call. Effects: E1 one replay-stable Message id; E2 distinct
        // committed ids; E3 reject missing authority. Decision table:
        // R1=C1=>E1; R2=C2=>E2; R3=C3=>E2; R4=C4=>E3. The ToolBatch is
        // the only coordinate owner; a provider call id alone cannot mint E1.
        let make = |run: &str, step| {
            ToolBatch::for_step(
                RunId(run.into()),
                step,
                ["voice-final", "another"].map(|call_id| {
                    (
                        ToolCall {
                            call_id: call_id.into(),
                            tool_id: "voice".into(),
                            arguments: serde_json::json!({}),
                        },
                        ToolRecoveryPolicy::replay_safe(),
                    )
                }),
            )
            .unwrap()
        };
        let first = make("run-a", 0);
        let cold: ToolBatch = serde_json::from_slice(&serde_json::to_vec(&first).unwrap()).unwrap();
        let id = first.result_message_id("voice-final").unwrap();
        assert_eq!(id, cold.result_message_id("voice-final").unwrap(), "R1/E1");
        assert_ne!(
            id,
            make("run-a", 1).result_message_id("voice-final").unwrap(),
            "R2/E2"
        );
        assert_ne!(
            id,
            make("run-b", 0).result_message_id("voice-final").unwrap(),
            "R2/E2"
        );
        assert_ne!(id, first.result_message_id("another").unwrap(), "R3/E2");
        assert_eq!(
            first.result_message_id("absent"),
            Err(ToolBatchError::UnknownCall("absent".into())),
            "R4 absent call cannot mint an owned result"
        );
    }
}
