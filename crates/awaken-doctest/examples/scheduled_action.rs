//! `ScheduledActionSpec` definition + `TypedScheduledActionHandler` impl —
//! pins both shapes `reference/scheduled-actions.md` cites. The spec
//! declares the key, phase, and payload type; the handler consumes one
//! decoded payload per fire and returns a `StateCommand` of follow-up
//! mutations / effects.

use async_trait::async_trait;
use awaken::state::StateCommand;
use awaken::{Phase, PhaseContext, ScheduledActionSpec, TypedScheduledActionHandler};
use awaken_contract::StateError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingPayload {
    target: String,
    attempt: u32,
}

struct PingAfterTool;
impl ScheduledActionSpec for PingAfterTool {
    const KEY: &'static str = "doctest.ping_after_tool";
    const PHASE: Phase = Phase::AfterToolExecute;
    type Payload = PingPayload;
}

/// User-side handler. The runtime constructs a `PhaseContext` and calls
/// `handle_typed` once per scheduled action fire; the returned
/// `StateCommand` is folded into the round's atomic commit.
struct PingHandler;

#[async_trait]
impl TypedScheduledActionHandler<PingAfterTool> for PingHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        _payload: PingPayload,
    ) -> Result<StateCommand, StateError> {
        Ok(StateCommand::new())
    }
}

fn main() {
    // Spec metadata.
    assert_eq!(PingAfterTool::KEY, "doctest.ping_after_tool");
    assert_eq!(PingAfterTool::PHASE, Phase::AfterToolExecute);

    // Payload encode/decode — the contract the runtime uses when
    // shuttling between scheduler and handler.
    let payload = PingPayload {
        target: "https://example.test".into(),
        attempt: 1,
    };
    let json = PingAfterTool::encode_payload(&payload).expect("encode");
    let decoded: PingPayload = serde_json::from_value(json).expect("decode");
    assert_eq!(decoded, payload);

    // Handler exists as a trait-object — the runtime stores them this way
    // through `PluginRegistrar::register_scheduled_action`.
    let _handler: Box<dyn TypedScheduledActionHandler<PingAfterTool>> = Box::new(PingHandler);
}
