//! Single source of truth for all `AgentEvent` variants.
//!
//! The [`define_agent_events!`] macro generates from a single declaration:
//! - `AgentEvent` enum
//! - `AgentEventType` discriminant enum
//! - `event_type()` mapping
//! - Wire data structs (in private `data` module)
//! - `Serialize` / `Deserialize` impls

use super::envelope_meta::{
    clear_serialize_run_hint_if_matches, serialize_run_hint, set_serialize_run_hint,
    take_runtime_event_envelope_meta,
};
use super::wire::{from_data_value, to_data_value, EventEnvelope};
use crate::runtime::llm::TokenUsage;
use crate::runtime::tool_call::ToolResult;
use crate::runtime::TerminationReason;
use crate::runtime::ToolCallOutcome;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tirea_state::TrackedPatch;

const fn default_tool_call_outcome() -> ToolCallOutcome {
    ToolCallOutcome::Succeeded
}

/// Dispatches run-hint set/clear for envelope events during serialization.
macro_rules! run_hint_action {
    (set, $hint:expr) => {
        set_serialize_run_hint($hint.0, $hint.1);
    };
    (clear, $hint:expr) => {
        clear_serialize_run_hint_if_matches(&$hint.0, &$hint.1);
    };
}

/// Generates `AgentEvent`, `AgentEventType`, wire data structs, and serde impls
/// from a single variant declaration.
///
/// # Sections
///
/// - **`envelope`** — Variants whose `thread_id`/`run_id` are promoted to the
///   event envelope. Declared with `=> set` (sets run hint) or `=> clear` (clears
///   run hint) for serialize-time tracking.
/// - **`standard`** — Variants whose enum fields map 1:1 to wire data struct fields.
/// - **`wire_only`** — Like standard, but the data struct has additional computed
///   fields (specified in `wire_extra { ... }`) that exist on the wire but not in
///   the enum.
/// - **`no_data`** — Unit variants with no payload; serialized with `data: null`.
macro_rules! define_agent_events {
    (
        data_imports { $($import:item)* }

        envelope {
            $(
                $(#[$env_vattr:meta])*
                $EnvVariant:ident {
                    $($(#[$env_dattr:meta])* $env_dfield:ident : $env_dfty:ty),*
                    $(,)?
                } => $hint_action:ident
            ),* $(,)?
        }

        standard {
            $(
                $(#[$std_vattr:meta])*
                $StdVariant:ident {
                    $($(#[$std_fattr:meta])* $std_field:ident : $std_fty:ty),*
                    $(,)?
                }
            ),* $(,)?
        }

        wire_only {
            $(
                $(#[$wire_vattr:meta])*
                $WireVariant:ident {
                    $($(#[$wire_fattr:meta])* $wire_field:ident : $wire_fty:ty),*
                    $(,)?
                } wire_extra {
                    $($(#[$wire_eattr:meta])* $wire_efield:ident : $wire_efty:ty = $wire_expr:expr),*
                    $(,)?
                }
            ),* $(,)?
        }

        no_data {
            $(
                $(#[$nd_vattr:meta])*
                $NoDataVariant:ident
            ),* $(,)?
        }
    ) => {
        // ======================== AgentEvent enum ========================

        /// Agent loop events for streaming execution.
        #[derive(Debug, Clone)]
        pub enum AgentEvent {
            $(
                $(#[$env_vattr])*
                $EnvVariant {
                    thread_id: String,
                    run_id: String,
                    $($env_dfield: $env_dfty),*
                },
            )*
            $(
                $(#[$std_vattr])*
                $StdVariant { $($std_field: $std_fty),* },
            )*
            $(
                $(#[$wire_vattr])*
                $WireVariant { $($wire_field: $wire_fty),* },
            )*
            $(
                $(#[$nd_vattr])*
                $NoDataVariant,
            )*
        }

        // ======================== AgentEventType enum ========================

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub(crate) enum AgentEventType {
            $($EnvVariant,)*
            $($StdVariant,)*
            $($WireVariant,)*
            $($NoDataVariant,)*
        }

        // ======================== event_type() ========================

        impl AgentEvent {
            pub(crate) fn event_type(&self) -> AgentEventType {
                match self {
                    $(Self::$EnvVariant { .. } => AgentEventType::$EnvVariant,)*
                    $(Self::$StdVariant { .. } => AgentEventType::$StdVariant,)*
                    $(Self::$WireVariant { .. } => AgentEventType::$WireVariant,)*
                    $(Self::$NoDataVariant => AgentEventType::$NoDataVariant,)*
                }
            }
        }

        // ======================== Wire data structs ========================

        mod data {
            $($import)*

            $(
                #[derive(Debug, Clone, Serialize, Deserialize)]
                pub(super) struct $EnvVariant {
                    $($(#[$env_dattr])* pub $env_dfield: $env_dfty,)*
                }
            )*

            $(
                #[derive(Debug, Clone, Serialize, Deserialize)]
                pub(super) struct $StdVariant {
                    $($(#[$std_fattr])* pub $std_field: $std_fty,)*
                }
            )*

            $(
                #[derive(Debug, Clone, Serialize, Deserialize)]
                pub(super) struct $WireVariant {
                    $($(#[$wire_fattr])* pub $wire_field: $wire_fty,)*
                    $($(#[$wire_eattr])* pub $wire_efield: $wire_efty,)*
                }
            )*
        }

        // ======================== Serialize ========================

        impl Serialize for AgentEvent {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let event_type = self.event_type();

                let run_hint = match self {
                    $(
                        Self::$EnvVariant { run_id, thread_id, .. } => {
                            Some((run_id.clone(), thread_id.clone()))
                        }
                    )*
                    _ => serialize_run_hint(),
                };

                let runtime_meta = take_runtime_event_envelope_meta(
                    event_type,
                    run_hint
                        .as_ref()
                        .map(|(r, t)| (r.as_str(), t.as_str())),
                );
                let meta_run_id = || runtime_meta.as_ref().map(|m| m.run_id.clone());
                let meta_thread_id = || runtime_meta.as_ref().map(|m| m.thread_id.clone());
                let meta_seq = || runtime_meta.as_ref().map(|m| m.seq);
                let meta_timestamp_ms = || runtime_meta.as_ref().map(|m| m.timestamp_ms);
                let meta_step_id = || runtime_meta.as_ref().and_then(|m| m.step_id.clone());

                let envelope = match self {
                    $(
                        Self::$EnvVariant { thread_id, run_id, $($env_dfield),* } => EventEnvelope {
                            event_type: AgentEventType::$EnvVariant,
                            run_id: meta_run_id().or_else(|| Some(run_id.clone())),
                            thread_id: meta_thread_id().or_else(|| Some(thread_id.clone())),
                            seq: meta_seq(),
                            timestamp_ms: meta_timestamp_ms(),
                            step_id: meta_step_id(),
                            data: to_data_value(&data::$EnvVariant {
                                $($env_dfield: $env_dfield.clone(),)*
                            }).map_err(serde::ser::Error::custom)?,
                        },
                    )*
                    $(
                        Self::$StdVariant { $($std_field),* } => EventEnvelope {
                            event_type: AgentEventType::$StdVariant,
                            run_id: meta_run_id(),
                            thread_id: meta_thread_id(),
                            seq: meta_seq(),
                            timestamp_ms: meta_timestamp_ms(),
                            step_id: meta_step_id(),
                            data: to_data_value(&data::$StdVariant {
                                $($std_field: $std_field.clone(),)*
                            }).map_err(serde::ser::Error::custom)?,
                        },
                    )*
                    $(
                        Self::$WireVariant { $($wire_field),* } => EventEnvelope {
                            event_type: AgentEventType::$WireVariant,
                            run_id: meta_run_id(),
                            thread_id: meta_thread_id(),
                            seq: meta_seq(),
                            timestamp_ms: meta_timestamp_ms(),
                            step_id: meta_step_id(),
                            data: to_data_value(&data::$WireVariant {
                                $($wire_field: $wire_field.clone(),)*
                                $($wire_efield: $wire_expr,)*
                            }).map_err(serde::ser::Error::custom)?,
                        },
                    )*
                    $(
                        Self::$NoDataVariant => EventEnvelope {
                            event_type: AgentEventType::$NoDataVariant,
                            run_id: meta_run_id(),
                            thread_id: meta_thread_id(),
                            seq: meta_seq(),
                            timestamp_ms: meta_timestamp_ms(),
                            step_id: meta_step_id(),
                            data: None,
                        },
                    )*
                };

                // Run hint management for envelope events.
                match self {
                    $(
                        Self::$EnvVariant { run_id, thread_id, .. } => {
                            let hint = runtime_meta
                                .as_ref()
                                .map(|meta| (meta.run_id.clone(), meta.thread_id.clone()))
                                .unwrap_or_else(|| (run_id.clone(), thread_id.clone()));
                            run_hint_action!($hint_action, hint);
                        }
                    )*
                    _ => {}
                }

                envelope.serialize(serializer)
            }
        }

        // ======================== Deserialize ========================

        impl<'de> Deserialize<'de> for AgentEvent {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let envelope = EventEnvelope::deserialize(deserializer)?;
                match envelope.event_type {
                    $(
                        AgentEventType::$EnvVariant => {
                            let d: data::$EnvVariant = from_data_value(envelope.data)?;
                            let run_id = envelope.run_id.ok_or_else(|| {
                                serde::de::Error::custom(concat!(
                                    stringify!($EnvVariant), " missing run_id"
                                ))
                            })?;
                            let thread_id = envelope.thread_id.ok_or_else(|| {
                                serde::de::Error::custom(concat!(
                                    stringify!($EnvVariant), " missing thread_id"
                                ))
                            })?;
                            Ok(Self::$EnvVariant {
                                thread_id,
                                run_id,
                                $($env_dfield: d.$env_dfield),*
                            })
                        }
                    )*
                    $(
                        AgentEventType::$StdVariant => {
                            let d: data::$StdVariant = from_data_value(envelope.data)?;
                            Ok(Self::$StdVariant { $($std_field: d.$std_field),* })
                        }
                    )*
                    $(
                        AgentEventType::$WireVariant => {
                            let d: data::$WireVariant = from_data_value(envelope.data)?;
                            $(let _ = d.$wire_efield;)*
                            Ok(Self::$WireVariant { $($wire_field: d.$wire_field),* })
                        }
                    )*
                    $(
                        AgentEventType::$NoDataVariant => Ok(Self::$NoDataVariant),
                    )*
                }
            }
        }
    };
}

// ============================================================================
// Event declarations — the single source of truth.
//
// To add a new event variant:
//   1. Add the declaration below in the appropriate section
//   2. That's it — the macro generates everything else
// ============================================================================

define_agent_events! {
    data_imports {
        use crate::runtime::TerminationReason;
        use crate::runtime::ToolCallOutcome;
        use crate::runtime::llm::TokenUsage;
        use crate::runtime::tool_call::ToolResult;
        use serde::{Deserialize, Serialize};
        use serde_json::Value;
        use tirea_state::TrackedPatch;
    }

    envelope {
        /// Run started.
        RunStart {
            #[serde(skip_serializing_if = "Option::is_none")]
            parent_run_id: Option<String>,
        } => set,

        /// Run finished.
        RunFinish {
            #[serde(skip_serializing_if = "Option::is_none")]
            result: Option<Value>,
            /// Why this run terminated.
            termination: TerminationReason,
        } => clear,
    }

    standard {
        /// LLM text delta.
        TextDelta { delta: String },

        /// LLM reasoning delta.
        ReasoningDelta { delta: String },

        /// Opaque reasoning signature/token delta from provider.
        ReasoningEncryptedValue { encrypted_value: String },

        /// Tool call started.
        ToolCallStart { id: String, name: String },

        /// Tool call arguments delta.
        ToolCallDelta { id: String, args_delta: String },

        /// Tool call input is complete.
        ToolCallReady { id: String, name: String, arguments: Value },

        /// Step started.
        StepStart {
            /// Pre-generated ID for the assistant message this step will produce.
            #[serde(default)]
            message_id: String,
        },

        /// LLM inference completed with token usage data.
        InferenceComplete {
            /// Model used for this inference.
            model: String,
            /// Token usage.
            #[serde(skip_serializing_if = "Option::is_none")]
            usage: Option<TokenUsage>,
            /// Duration of the LLM call in milliseconds.
            duration_ms: u64,
        },

        /// State snapshot.
        StateSnapshot { snapshot: Value },

        /// State delta.
        StateDelta { delta: Vec<Value> },

        /// Messages snapshot.
        MessagesSnapshot { messages: Vec<Value> },

        /// Activity snapshot.
        ActivitySnapshot {
            message_id: String,
            activity_type: String,
            content: Value,
            #[serde(skip_serializing_if = "Option::is_none")]
            replace: Option<bool>,
        },

        /// Activity delta.
        ActivityDelta {
            message_id: String,
            activity_type: String,
            patch: Vec<Value>,
        },

        /// Suspension resolution received.
        ToolCallResumed { target_id: String, result: Value },

        /// Error occurred.
        Error {
            message: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            code: Option<String>,
        },
    }

    wire_only {
        /// Tool call completed.
        ToolCallDone {
            id: String,
            result: ToolResult,
            #[serde(skip_serializing_if = "Option::is_none")]
            patch: Option<TrackedPatch>,
            /// Pre-generated ID for the stored tool result message.
            #[serde(default)]
            message_id: String,
        } wire_extra {
            #[serde(default = "super::default_tool_call_outcome")]
            outcome: ToolCallOutcome = ToolCallOutcome::from_tool_result(result),
        }
    }

    no_data {
        /// Step completed.
        StepEnd,
    }
}

// ============================================================================
// Hand-written impls that don't belong in the macro.
// ============================================================================

impl AgentEvent {
    /// Extract the response text from a `RunFinish` result value.
    pub fn extract_response(result: &Option<Value>) -> String {
        result
            .as_ref()
            .and_then(|v| v.get("response"))
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_string()
    }
}
