//! Same-run awaiting state and the durable resume ticket.
//!
//! When a run awaits (a tool needs a decision, external input, a timer, …) it
//! commits a [`ResumeTicket`]: the structured correlation a later resume must
//! match before the run continues. The ticket is pure agent-domain data so it
//! survives a commit and is validated on resume, never a live handle.

use serde::{Deserialize, Deserializer, Serialize};

/// Why a run is awaiting. A client-executed tool is just one awaiting reason — the
/// design keeps these neutral rather than naming an "external tool" concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AwaitReason {
    ToolPermission,
    UserInput,
    ExternalEvent,
    RateLimit,
    ManualPause,
    /// The run committed a `ScheduledAction` (ADR-0003 mechanism #1): a deferred
    /// action recorded in committed state, performed by the system (not decided
    /// by a human) and recovered from the committed request for consistency
    /// (ADR-0020).
    ScheduledAction,
    /// A delegated sub-agent needs more input. Its opaque execution reference
    /// is owned by the durable parent/child relationship, not duplicated here.
    Delegation,
}

/// A tool call held at a durable awaiting boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingTool {
    pub tool_id: String,
    pub arguments: serde_json::Value,
}

/// Why a pending tool call is awaiting. Each variant necessarily carries both
/// the correlation call id and the call itself through [`AwaitTarget::ToolCall`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolAwaitReason {
    Permission,
    ClientExecution,
    ScheduledAction,
    Delegation,
}

/// Why a remote agent is awaiting caller input without a local tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteInputReason {
    UserInput,
    ExternalEvent,
}

/// Why an operator/system pause has no pending call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    Manual,
    RateLimit,
}

/// The closed payload of an awaiting ticket. Optional `call_id` and
/// `pending_tool` fields are deliberately replaced by sum types: every variant
/// contains exactly the facts its resume path requires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AwaitTarget {
    ToolCall {
        reason: ToolAwaitReason,
        call_id: String,
        tool: PendingTool,
    },
    RemoteInput {
        reason: RemoteInputReason,
        call_id: String,
    },
    Pause(PauseReason),
}

impl AwaitTarget {
    #[must_use]
    pub const fn reason(&self) -> AwaitReason {
        match self {
            Self::ToolCall { reason, .. } => match reason {
                ToolAwaitReason::Permission => AwaitReason::ToolPermission,
                ToolAwaitReason::ClientExecution => AwaitReason::ExternalEvent,
                ToolAwaitReason::ScheduledAction => AwaitReason::ScheduledAction,
                ToolAwaitReason::Delegation => AwaitReason::Delegation,
            },
            Self::RemoteInput { reason, .. } => match reason {
                RemoteInputReason::UserInput => AwaitReason::UserInput,
                RemoteInputReason::ExternalEvent => AwaitReason::ExternalEvent,
            },
            Self::Pause(PauseReason::Manual) => AwaitReason::ManualPause,
            Self::Pause(PauseReason::RateLimit) => AwaitReason::RateLimit,
        }
    }

    #[must_use]
    pub fn call_id(&self) -> Option<&str> {
        match self {
            Self::ToolCall { call_id, .. } | Self::RemoteInput { call_id, .. } => Some(call_id),
            Self::Pause(_) => None,
        }
    }

    #[must_use]
    pub fn pending_tool(&self) -> Option<&PendingTool> {
        match self {
            Self::ToolCall { tool, .. } => Some(tool),
            Self::RemoteInput { .. } | Self::Pause(_) => None,
        }
    }

    #[must_use]
    pub fn tool_call(&self) -> Option<(&str, &PendingTool)> {
        match self {
            Self::ToolCall { call_id, tool, .. } => Some((call_id, tool)),
            Self::RemoteInput { .. } | Self::Pause(_) => None,
        }
    }
}

impl AwaitReason {
    /// The stable snake_case token emitted on the `Awaiting` stream event. Kept
    /// beside the enum so the wire vocabulary has one authoritative source and a
    /// new reason variant forces its token here, not at each await site.
    #[must_use]
    pub fn as_stream_str(&self) -> &'static str {
        match self {
            AwaitReason::ToolPermission => "tool_permission",
            AwaitReason::UserInput => "user_input",
            AwaitReason::ExternalEvent => "external_event",
            AwaitReason::RateLimit => "rate_limit",
            AwaitReason::ManualPause => "manual_pause",
            AwaitReason::ScheduledAction => "scheduled_action",
            AwaitReason::Delegation => "delegation",
        }
    }
}

/// The two legal permission outcomes shared by protocol adapters, session
/// application services, and the runtime. Both outcomes may carry explanatory
/// text, but the enum makes their distinct meaning explicit and exhaustive.
///
/// ```compile_fail
/// use awaken_agent_contract::agent::awaiting::PermissionDecision;
///
/// let _ = PermissionDecision { allow: true, note: None };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// The committed correlation for one same-run pause. A resume is accepted only
/// when its correlation, run/thread, executable snapshot, and catalog
/// fingerprint all match, and the deadline (if any) has not passed.
///
/// The awaiting payload is private and can only be supplied as one closed
/// [`AwaitTarget`], so the former independent optional call/tool fields cannot
/// be assembled into contradictory states.
///
/// ```compile_fail
/// use awaken_agent_contract::agent::awaiting::{AwaitReason, ResumeTicket};
/// use awaken_agent_contract::agent::{run, thread};
///
/// let _ = ResumeTicket {
///     correlation_id: "c".into(),
///     run_id: run::Id("r".into()),
///     thread_id: thread::Id("t".into()),
///     snapshot_id: "s".into(),
///     catalog_fingerprint: "f".into(),
///     delegation_origin: None,
///     data_subject_id: None,
///     reason: AwaitReason::ToolPermission,
///     call_id: None,
///     pending_tool: None,
///     deadline_ms: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResumeTicket {
    /// Idempotency/correlation key; deduplicates retries and duplicate wakes.
    pub correlation_id: String,
    pub run_id: crate::agent::run::Id,
    pub thread_id: crate::agent::thread::Id,
    /// Executable snapshot id and catalog fingerprint, proving a resume result
    /// belongs to the same executable configuration (held as plain ids so the
    /// agent-domain contract stays independent of runtime-facing types).
    pub snapshot_id: String,
    pub catalog_fingerprint: String,
    /// Stable origin of this Run. A delegated Run keeps it across every await,
    /// process restart, and resume; a directly admitted Run stores `None`.
    #[serde(default, alias = "initiator", skip_serializing_if = "Option::is_none")]
    pub delegation_origin: Option<crate::agent::delegation::DelegationOrigin>,
    /// Request-grained neutral content owner retained across same-Run resume.
    /// Kept as a plain id so the agent contract does not depend on runtime types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_subject_id: Option<String>,
    target: AwaitTarget,
    /// Optional expiry (epoch millis). A resume after this is stale.
    pub deadline_ms: Option<u64>,
}

#[derive(Deserialize)]
struct ResumeTicketWire {
    correlation_id: String,
    run_id: crate::agent::run::Id,
    thread_id: crate::agent::thread::Id,
    snapshot_id: String,
    catalog_fingerprint: String,
    #[serde(default, alias = "initiator")]
    delegation_origin: Option<crate::agent::delegation::DelegationOrigin>,
    #[serde(default)]
    data_subject_id: Option<String>,
    #[serde(default)]
    target: Option<AwaitTarget>,
    #[serde(default)]
    reason: Option<AwaitReason>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    pending_tool: Option<PendingTool>,
    deadline_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for ResumeTicket {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ResumeTicketWire::deserialize(deserializer)?;
        let target = match (wire.target, wire.reason, wire.call_id, wire.pending_tool) {
            (Some(target), None, None, None) => target,
            (None, Some(AwaitReason::ToolPermission), Some(call_id), Some(tool)) => {
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::Permission,
                    call_id,
                    tool,
                }
            }
            (None, Some(AwaitReason::ExternalEvent), Some(call_id), Some(tool)) => {
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::ClientExecution,
                    call_id,
                    tool,
                }
            }
            (None, Some(AwaitReason::ScheduledAction), Some(call_id), Some(tool)) => {
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::ScheduledAction,
                    call_id,
                    tool,
                }
            }
            (None, Some(AwaitReason::Delegation), Some(call_id), Some(tool)) => {
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::Delegation,
                    call_id,
                    tool,
                }
            }
            (None, Some(AwaitReason::UserInput), Some(call_id), None) => AwaitTarget::RemoteInput {
                reason: RemoteInputReason::UserInput,
                call_id,
            },
            (None, Some(AwaitReason::ExternalEvent), Some(call_id), None) => {
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::ExternalEvent,
                    call_id,
                }
            }
            (None, Some(AwaitReason::ManualPause), None, None) => {
                AwaitTarget::Pause(PauseReason::Manual)
            }
            (None, Some(AwaitReason::RateLimit), None, None) => {
                AwaitTarget::Pause(PauseReason::RateLimit)
            }
            _ => {
                return Err(serde::de::Error::custom(
                    "resume ticket must contain one valid closed await target",
                ));
            }
        };
        Ok(Self {
            correlation_id: wire.correlation_id,
            run_id: wire.run_id,
            thread_id: wire.thread_id,
            snapshot_id: wire.snapshot_id,
            catalog_fingerprint: wire.catalog_fingerprint,
            delegation_origin: wire.delegation_origin,
            data_subject_id: wire.data_subject_id,
            target,
            deadline_ms: wire.deadline_ms,
        })
    }
}

impl ResumeTicket {
    pub fn new(
        correlation_id: impl Into<String>,
        run_id: crate::agent::run::Id,
        thread_id: crate::agent::thread::Id,
        snapshot_id: impl Into<String>,
        catalog_fingerprint: impl Into<String>,
        target: AwaitTarget,
    ) -> Self {
        Self {
            correlation_id: correlation_id.into(),
            run_id,
            thread_id,
            snapshot_id: snapshot_id.into(),
            catalog_fingerprint: catalog_fingerprint.into(),
            delegation_origin: None,
            data_subject_id: None,
            target,
            deadline_ms: None,
        }
    }

    #[must_use]
    pub fn with_delegation_origin(
        mut self,
        origin: Option<crate::agent::delegation::DelegationOrigin>,
    ) -> Self {
        self.delegation_origin = origin;
        self
    }

    #[must_use]
    pub fn with_data_subject(mut self, data_subject_id: Option<String>) -> Self {
        self.data_subject_id = data_subject_id;
        self
    }

    #[must_use]
    pub fn with_deadline(mut self, deadline_ms: Option<u64>) -> Self {
        self.deadline_ms = deadline_ms;
        self
    }

    pub fn bind_thread(&mut self, thread_id: crate::agent::thread::Id) {
        self.thread_id = thread_id;
    }

    #[must_use]
    pub const fn reason(&self) -> AwaitReason {
        self.target.reason()
    }

    #[must_use]
    pub fn call_id(&self) -> Option<&str> {
        self.target.call_id()
    }

    #[must_use]
    pub fn pending_tool(&self) -> Option<&PendingTool> {
        self.target.pending_tool()
    }

    #[must_use]
    pub const fn target(&self) -> &AwaitTarget {
        &self.target
    }

    #[must_use]
    pub fn tool_call(&self) -> Option<(&str, &PendingTool)> {
        self.target.tool_call()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_has_a_distinct_stable_stream_token() {
        let all = [
            (AwaitReason::ToolPermission, "tool_permission"),
            (AwaitReason::UserInput, "user_input"),
            (AwaitReason::ExternalEvent, "external_event"),
            (AwaitReason::RateLimit, "rate_limit"),
            (AwaitReason::ManualPause, "manual_pause"),
            (AwaitReason::ScheduledAction, "scheduled_action"),
            (AwaitReason::Delegation, "delegation"),
        ];
        for (reason, token) in &all {
            assert_eq!(reason.as_stream_str(), *token);
        }
        // Tokens are unique across the closed set.
        let mut tokens: Vec<&str> = all.iter().map(|(_, t)| *t).collect();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), all.len(), "stream tokens must be unique");
    }

    #[test]
    fn every_closed_target_projects_and_round_trips() {
        let pt = PendingTool {
            tool_id: "t".into(),
            arguments: serde_json::json!({"a": 1}),
        };
        let cases = [
            (
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::Permission,
                    call_id: "tool-call".into(),
                    tool: pt.clone(),
                },
                AwaitReason::ToolPermission,
                Some("tool-call"),
                true,
            ),
            (
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::ClientExecution,
                    call_id: "client-call".into(),
                    tool: pt.clone(),
                },
                AwaitReason::ExternalEvent,
                Some("client-call"),
                true,
            ),
            (
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::ScheduledAction,
                    call_id: "scheduled-call".into(),
                    tool: pt.clone(),
                },
                AwaitReason::ScheduledAction,
                Some("scheduled-call"),
                true,
            ),
            (
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::Delegation,
                    call_id: "delegated-call".into(),
                    tool: pt,
                },
                AwaitReason::Delegation,
                Some("delegated-call"),
                true,
            ),
            (
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::UserInput,
                    call_id: "remote-task".into(),
                },
                AwaitReason::UserInput,
                Some("remote-task"),
                false,
            ),
            (
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::ExternalEvent,
                    call_id: "remote-auth".into(),
                },
                AwaitReason::ExternalEvent,
                Some("remote-auth"),
                false,
            ),
            (
                AwaitTarget::Pause(PauseReason::Manual),
                AwaitReason::ManualPause,
                None,
                false,
            ),
            (
                AwaitTarget::Pause(PauseReason::RateLimit),
                AwaitReason::RateLimit,
                None,
                false,
            ),
        ];

        for (target, reason, call_id, has_tool) in cases {
            let ticket = ResumeTicket::new(
                "c",
                crate::agent::run::Id("r".into()),
                crate::agent::thread::Id("th".into()),
                "s",
                "f",
                target,
            )
            .with_deadline(Some(42));
            assert_eq!(ticket.reason(), reason);
            assert_eq!(ticket.call_id(), call_id);
            assert_eq!(ticket.pending_tool().is_some(), has_tool);
            assert_eq!(ticket.tool_call().is_some(), has_tool);

            let back: ResumeTicket =
                serde_json::from_str(&serde_json::to_string(&ticket).unwrap()).unwrap();
            assert_eq!(back, ticket);
        }
    }

    #[test]
    fn valid_persisted_optional_payloads_converge_to_closed_targets() {
        // Cause/effect decision table:
        // R1: one canonical target and no legacy facts -> preserve the target;
        // R2: one historically valid reason/call/tool product -> construct the
        //     single equivalent closed target used by all current callers;
        // R3: a missing required fact, a forbidden extra fact, or two target
        //     representations -> reject before durable runtime hydration.
        // The historical decoder is deliberately owned here, not duplicated
        // across Memory, filesystem, SQLite, and Postgres readers.
        let pending_tool = || PendingTool {
            tool_id: "tool-1".into(),
            arguments: serde_json::json!({"cmd": "true"}),
        };
        let valid = vec![
            (
                serde_json::json!({
                    "reason": "ToolPermission", "call_id": "permission",
                    "pending_tool": {"tool_id": "tool-1", "arguments": {"cmd": "true"}}
                }),
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::Permission,
                    call_id: "permission".into(),
                    tool: pending_tool(),
                },
            ),
            (
                serde_json::json!({
                    "reason": "ExternalEvent", "call_id": "client",
                    "pending_tool": {"tool_id": "tool-1", "arguments": {"cmd": "true"}}
                }),
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::ClientExecution,
                    call_id: "client".into(),
                    tool: pending_tool(),
                },
            ),
            (
                serde_json::json!({
                    "reason": "ScheduledAction", "call_id": "scheduled",
                    "pending_tool": {"tool_id": "tool-1", "arguments": {"cmd": "true"}}
                }),
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::ScheduledAction,
                    call_id: "scheduled".into(),
                    tool: pending_tool(),
                },
            ),
            (
                serde_json::json!({
                    "reason": "Delegation", "call_id": "delegation",
                    "pending_tool": {"tool_id": "tool-1", "arguments": {"cmd": "true"}}
                }),
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::Delegation,
                    call_id: "delegation".into(),
                    tool: pending_tool(),
                },
            ),
            (
                serde_json::json!({"reason": "UserInput", "call_id": "input"}),
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::UserInput,
                    call_id: "input".into(),
                },
            ),
            (
                serde_json::json!({"reason": "ExternalEvent", "call_id": "remote"}),
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::ExternalEvent,
                    call_id: "remote".into(),
                },
            ),
            (
                serde_json::json!({"reason": "ManualPause"}),
                AwaitTarget::Pause(PauseReason::Manual),
            ),
            (
                serde_json::json!({"reason": "RateLimit"}),
                AwaitTarget::Pause(PauseReason::RateLimit),
            ),
        ];
        for (mut legacy_fields, expected) in valid {
            let mut legacy = serde_json::json!({
                "correlation_id": "c",
                "run_id": "r",
                "thread_id": "th",
                "snapshot_id": "s",
                "catalog_fingerprint": "f",
                "deadline_ms": null
            });
            legacy
                .as_object_mut()
                .unwrap()
                .append(legacy_fields.as_object_mut().unwrap());
            let ticket = serde_json::from_value::<ResumeTicket>(legacy).unwrap();
            assert_eq!(ticket.target(), &expected);
        }

        let invalid = [
            serde_json::json!({"reason": "ToolPermission", "call_id": "call"}),
            serde_json::json!({
                "reason": "UserInput", "call_id": "input",
                "pending_tool": {"tool_id": "tool-1", "arguments": {}}
            }),
            serde_json::json!({"reason": "ManualPause", "call_id": "call"}),
            serde_json::json!({
                "target": {"Pause": "Manual"}, "reason": "ManualPause"
            }),
        ];
        for mut invalid_fields in invalid {
            let mut invalid = serde_json::json!({
                "correlation_id": "c",
                "run_id": "r",
                "thread_id": "th",
                "snapshot_id": "s",
                "catalog_fingerprint": "f",
                "deadline_ms": null
            });
            invalid
                .as_object_mut()
                .unwrap()
                .append(invalid_fields.as_object_mut().unwrap());
            assert!(serde_json::from_value::<ResumeTicket>(invalid).is_err());
        }
    }
}
