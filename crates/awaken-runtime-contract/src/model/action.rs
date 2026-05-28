use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::StateError;
use crate::state::StateKey;

use super::{JsonValue, Phase, decode_json, encode_json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledAction {
    pub phase: Phase,
    pub key: String,
    pub payload: JsonValue,
}

impl ScheduledAction {
    pub fn new(phase: Phase, key: impl Into<String>, payload: JsonValue) -> Self {
        Self {
            phase,
            key: key.into(),
            payload,
        }
    }
}

pub trait ScheduledActionSpec: 'static + Send + Sync {
    const KEY: &'static str;
    const PHASE: Phase;

    type Payload: Serialize + DeserializeOwned + Send + Sync + 'static;

    fn encode_payload(payload: &Self::Payload) -> Result<JsonValue, StateError> {
        encode_json(Self::KEY, payload)
    }

    fn decode_payload(payload: JsonValue) -> Result<Self::Payload, StateError> {
        decode_json(Self::KEY, payload)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledActionEnvelope {
    pub id: u64,
    pub action: ScheduledAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedScheduledAction {
    pub id: u64,
    pub action: ScheduledAction,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ScheduledActionQueueUpdate {
    Push(ScheduledActionEnvelope),
    Remove { id: u64 },
}

pub struct PendingScheduledActions;

impl StateKey for PendingScheduledActions {
    const KEY: &'static str = "__runtime.pending_scheduled_actions";

    type Value = Vec<ScheduledActionEnvelope>;
    type Update = ScheduledActionQueueUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            ScheduledActionQueueUpdate::Push(entry) => value.push(entry),
            ScheduledActionQueueUpdate::Remove { id } => value.retain(|entry| entry.id != id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FailedScheduledActionUpdate {
    Push(FailedScheduledAction),
    Remove { id: u64 },
}

pub struct FailedScheduledActions;

impl StateKey for FailedScheduledActions {
    const KEY: &'static str = "__runtime.failed_scheduled_actions";

    type Value = Vec<FailedScheduledAction>;
    type Update = FailedScheduledActionUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            FailedScheduledActionUpdate::Push(entry) => value.push(entry),
            FailedScheduledActionUpdate::Remove { id } => value.retain(|entry| entry.id != id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestAction;

    impl ScheduledActionSpec for TestAction {
        const KEY: &'static str = "test.action";
        const PHASE: Phase = Phase::BeforeInference;
        type Payload = String;
    }

    #[test]
    fn scheduled_action_spec_round_trip_works() {
        let payload = "hello".to_string();
        let encoded = TestAction::encode_payload(&payload).expect("encode should succeed");
        let decoded = TestAction::decode_payload(encoded).expect("decode should succeed");

        assert_eq!(decoded, payload);
    }
}
