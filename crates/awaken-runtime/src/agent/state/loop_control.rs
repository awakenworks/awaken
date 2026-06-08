//! Plugin-driven loop control directives consumed at natural-end boundaries.

use awaken_runtime_contract::contract::message::Message;
use serde::{Deserialize, Serialize};

use crate::state::StateKey;

/// A plugin directive for the agent loop's next natural-end decision.
///
/// The runtime does not attach domain meaning to these variants. Plugins own
/// the reason strings and any message payloads; the loop runner only consumes
/// the directive at the natural-end boundary and translates it into generic
/// control flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoopControl {
    /// Keep the same run alive and append messages before the next inference.
    Continue {
        /// Stable, plugin-defined reason code.
        reason: String,
        /// Messages appended to the in-memory transcript before continuing.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        messages: Vec<Message>,
    },
    /// Finish the run because a plugin-controlled objective completed.
    Finish {
        /// Stable, plugin-defined reason code.
        reason: String,
        /// Optional plugin-defined result payload for observers that read the
        /// control state before it is consumed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
    },
    /// Pause the run in `Waiting` with the supplied reason.
    Wait {
        /// Stable, plugin-defined waiting reason.
        reason: String,
    },
    /// Terminate the run as an error with the supplied message.
    Fail {
        /// Stable, plugin-defined error reason.
        reason: String,
    },
}

impl LoopControl {
    /// Convenience constructor for the common "continue with feedback" case.
    #[must_use]
    pub fn continue_with(reason: impl Into<String>, messages: Vec<Message>) -> Self {
        Self::Continue {
            reason: reason.into(),
            messages,
        }
    }

    /// Convenience constructor for plugin-driven normal completion.
    #[must_use]
    pub fn finish(reason: impl Into<String>) -> Self {
        Self::Finish {
            reason: reason.into(),
            result: None,
        }
    }

    /// Convenience constructor for plugin-driven waiting.
    #[must_use]
    pub fn wait(reason: impl Into<String>) -> Self {
        Self::Wait {
            reason: reason.into(),
        }
    }

    /// Convenience constructor for plugin-driven failure.
    #[must_use]
    pub fn fail(reason: impl Into<String>) -> Self {
        Self::Fail {
            reason: reason.into(),
        }
    }
}

/// Update operation for [`LoopControlKey`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LoopControlUpdate {
    Set { directive: LoopControl },
    Clear,
}

/// Single pending loop-control directive.
///
/// The key uses the default `Exclusive` merge strategy so two plugins cannot
/// silently issue competing control directives in the same phase.
pub struct LoopControlKey;

impl StateKey for LoopControlKey {
    const KEY: &'static str = "__runtime.loop_control";

    type Value = Option<LoopControl>;
    type Update = LoopControlUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            LoopControlUpdate::Set { directive } => {
                if value.is_some() {
                    *value = Some(LoopControl::fail("loop_control_conflict"));
                } else {
                    *value = Some(directive);
                }
            }
            LoopControlUpdate::Clear => *value = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_clear_loop_control() {
        let mut value = None;
        LoopControlKey::apply(
            &mut value,
            LoopControlUpdate::Set {
                directive: LoopControl::continue_with(
                    "needs_revision",
                    vec![Message::internal_user("revise")],
                ),
            },
        );
        assert!(matches!(value, Some(LoopControl::Continue { .. })));

        LoopControlKey::apply(&mut value, LoopControlUpdate::Clear);
        assert_eq!(value, None);
    }

    #[test]
    fn duplicate_set_marks_conflict() {
        let mut value = None;
        LoopControlKey::apply(
            &mut value,
            LoopControlUpdate::Set {
                directive: LoopControl::finish("first"),
            },
        );
        LoopControlKey::apply(
            &mut value,
            LoopControlUpdate::Set {
                directive: LoopControl::finish("second"),
            },
        );

        assert!(matches!(
            value,
            Some(LoopControl::Fail { reason }) if reason == "loop_control_conflict"
        ));
    }

    #[test]
    fn clear_then_set_allows_replacement() {
        let mut value = None;
        LoopControlKey::apply(
            &mut value,
            LoopControlUpdate::Set {
                directive: LoopControl::finish("first"),
            },
        );
        LoopControlKey::apply(&mut value, LoopControlUpdate::Clear);
        LoopControlKey::apply(
            &mut value,
            LoopControlUpdate::Set {
                directive: LoopControl::finish("second"),
            },
        );

        assert!(matches!(
            value,
            Some(LoopControl::Finish { reason, .. }) if reason == "second"
        ));
    }
}
