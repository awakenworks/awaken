//! What each agent-visible fold of a committed transcript is: its Managed tool
//! family, its public identity, and which committed User input is public.
//!
//! Consumers: the protocol adapter's transcoder (committed folds and the live
//! stream share one classifier) and its root and child transcript projectors.

use std::collections::HashSet;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Record as RunRecord;
use awaken_agent_contract::event::{Fact, ToolDisposition};
use awaken_session_contract::SessionEventBatch;

use crate::{EventIdentity, FactProvenance, ManagedEventType, ToolResultSource, TranscriptRole};

/// The public tool family of one committed call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolUseFamily {
    /// Host-executed by the Agent toolset.
    Agent,
    /// Host-executed through an MCP server (`mcp__<server>__<tool>`).
    Mcp,
    /// Executed by the client, which supplies the result.
    Client,
}

/// One agent-visible fold of a committed transcript, classified by tool
/// family. `awaiting_decision` is the only permission a transcript can
/// establish on its own: the committed ticket says a person was asked. A call
/// with a result establishes nothing, since a refused call and an executed
/// call whose tool errored look identical here.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptFold {
    Thinking,
    Message {
        content: Vec<ContentBlock>,
    },
    ToolUse {
        call_id: String,
        name: String,
        input: serde_json::Value,
        family: ToolUseFamily,
        awaiting_decision: bool,
    },
    ToolResult {
        call_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
        mcp: bool,
    },
}

/// The MCP calls seen so far in one fold, so each result takes its own call's
/// family. A reused provider-local call id takes the family of its latest call.
#[derive(Clone, Debug, Default)]
pub struct ToolFamilies {
    mcp_calls: HashSet<String>,
}

impl ToolFamilies {
    /// Families for a fold whose results answer `sources`, the exact
    /// committed calls resolved for them.
    #[must_use]
    pub fn for_result_sources(sources: &[ToolResultSource]) -> Self {
        Self::with_mcp_calls(
            sources
                .iter()
                .filter(|source| awaken_agent_contract::McpToolId::claims_namespace(&source.name))
                .map(|source| source.call_id.clone()),
        )
    }

    /// Families for a fold that continues after these MCP calls.
    #[must_use]
    pub fn with_mcp_calls(calls: impl IntoIterator<Item = String>) -> Self {
        Self {
            mcp_calls: calls.into_iter().collect(),
        }
    }

    /// Classify one fact. Run boundaries, pauses and continuation rounds are
    /// not transcript folds (`None`).
    pub fn classify(&mut self, fact: &Fact) -> Option<TranscriptFold> {
        Some(match fact {
            Fact::AssistantThinking => TranscriptFold::Thinking,
            Fact::AssistantMessage { content, .. } => TranscriptFold::Message {
                content: content.clone(),
            },
            Fact::ToolCall {
                id,
                name,
                input,
                disposition,
            } => {
                let family = if awaken_agent_contract::McpToolId::claims_namespace(name) {
                    self.mcp_calls.insert(id.clone());
                    ToolUseFamily::Mcp
                } else {
                    self.mcp_calls.remove(id);
                    match disposition {
                        ToolDisposition::PendingClient => ToolUseFamily::Client,
                        ToolDisposition::PendingBuiltin | ToolDisposition::Executed => {
                            ToolUseFamily::Agent
                        }
                    }
                };
                TranscriptFold::ToolUse {
                    call_id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    family,
                    awaiting_decision: matches!(disposition, ToolDisposition::PendingBuiltin),
                }
            }
            Fact::ToolResult {
                id,
                content,
                is_error,
            } => TranscriptFold::ToolResult {
                call_id: id.clone(),
                content: content.clone(),
                is_error: *is_error,
                mcp: self.mcp_calls.contains(id),
            },
            Fact::RunStarted
            | Fact::Awaiting { .. }
            | Fact::RunFinished { .. }
            | Fact::RunFailed { .. }
            | Fact::Continuation { .. } => return None,
        })
    }
}

/// The public identity of one Message-identified transcript fold: an
/// assistant response when the Message carries Run coordinates, else the
/// Message and the fold's ordinal within it.
#[must_use]
pub fn transcript_fact_identity(
    thread_id: &str,
    message_id: &str,
    ordinal: usize,
    role: TranscriptRole,
    coordinates: Option<&(String, usize, usize)>,
) -> EventIdentity {
    let provenance = match (role, coordinates) {
        (
            TranscriptRole::AgentMessage | TranscriptRole::AgentThinking,
            Some((run_id, step, response)),
        ) => FactProvenance::AssistantResponse {
            run_id: run_id.clone(),
            step: *step,
            response: *response,
        },
        _ => FactProvenance::Message {
            message_id: message_id.to_string(),
            ordinal,
        },
    };
    EventIdentity::new(thread_id, ManagedEventType::Transcript(role), provenance)
}

/// Whether a committed primary-Thread Message is public User input that no
/// retained Session admission already owns. Internal child reports, steers and
/// continuations are never customer input; identity and role decide, never
/// text.
#[must_use]
pub fn is_public_user_input(
    message: &Message,
    session_id: &str,
    batches: &[SessionEventBatch],
    runs: &[RunRecord],
) -> bool {
    message.role == Role::User
        && !message.id.is_agent_thread_report()
        && !runs
            .iter()
            .any(|run| message.id.is_continuation_of(&run.id) || message.id.is_steer_of(&run.id))
        && !batches.iter().flat_map(|batch| &batch.events).any(|entry| {
            MessageId::session_event_input(session_id, entry.event.operation_id()) == message.id
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_fold_takes_its_own_call_family_and_only_a_pending_builtin_awaits() {
        // Causes: C1 call name in the MCP namespace or not; C2 disposition
        // pending-builtin, pending-client or executed; C3 a result follows
        // its call, a result's call id was seeded from exact sources, or a
        // later ordinary call reuses an MCP call id. Effects: E1 MCP names are
        // MCP whatever the disposition; E2 others are Client iff
        // pending-client, else Agent; E3 only pending-builtin awaits a
        // decision; E4 a result is MCP iff its latest call (or seeded source)
        // is; E5 run boundaries are not folds. Rules: R1=C1 x C2=>E1-E3;
        // R2=C3=>E4; R3=E5.
        let call = |name: &str, disposition| Fact::ToolCall {
            id: "call".into(),
            name: name.into(),
            input: serde_json::json!({}),
            disposition,
        };
        for (name, disposition, family, awaiting) in [
            (
                "mcp__srv__echo",
                ToolDisposition::PendingBuiltin,
                ToolUseFamily::Mcp,
                true,
            ),
            (
                "mcp__srv__echo",
                ToolDisposition::PendingClient,
                ToolUseFamily::Mcp,
                false,
            ),
            (
                "mcp__srv__echo",
                ToolDisposition::Executed,
                ToolUseFamily::Mcp,
                false,
            ),
            (
                "read",
                ToolDisposition::PendingBuiltin,
                ToolUseFamily::Agent,
                true,
            ),
            (
                "read",
                ToolDisposition::PendingClient,
                ToolUseFamily::Client,
                false,
            ),
            (
                "read",
                ToolDisposition::Executed,
                ToolUseFamily::Agent,
                false,
            ),
        ] {
            let label = format!("{disposition:?}");
            let fold = ToolFamilies::default().classify(&call(name, disposition));
            assert!(
                matches!(
                    fold,
                    Some(TranscriptFold::ToolUse { family: f, awaiting_decision: a, .. })
                        if f == family && a == awaiting
                ),
                "R1 {name} {label}"
            );
        }

        let result = Fact::ToolResult {
            id: "call".into(),
            content: Vec::new(),
            is_error: false,
        };
        let is_mcp = |families: &mut ToolFamilies| {
            matches!(
                families.classify(&result),
                Some(TranscriptFold::ToolResult { mcp: true, .. })
            )
        };
        let mut families = ToolFamilies::default();
        families.classify(&call("mcp__srv__echo", ToolDisposition::Executed));
        assert!(is_mcp(&mut families), "R2 follows its MCP call");
        families.classify(&call("read", ToolDisposition::Executed));
        assert!(!is_mcp(&mut families), "R2 reused id takes the latest call");
        let seeded = [ToolResultSource {
            source_id: "message".into(),
            call_id: "call".into(),
            name: "mcp__srv__echo".into(),
        }];
        assert!(
            is_mcp(&mut ToolFamilies::for_result_sources(&seeded)),
            "R2 seeded source"
        );
        assert!(
            ToolFamilies::default()
                .classify(&Fact::RunFinished { exhausted: false })
                .is_none(),
            "R3"
        );
    }

    #[test]
    fn assistant_folds_with_coordinates_are_response_identified() {
        // Causes: C1 the fold is an agent message or thinking; C2 the Message
        // carries Run coordinates. Effects: E1 C1+C2 is identified by its Run
        // response; E2 otherwise by Message and ordinal. Rules: R1=C1+C2=>E1;
        // R2=!C1|!C2=>E2.
        let coordinates = ("run".to_string(), 2, 1);
        for (role, coordinates, response) in [
            (TranscriptRole::AgentMessage, Some(&coordinates), true),
            (TranscriptRole::AgentThinking, Some(&coordinates), true),
            (TranscriptRole::AgentMessage, None, false),
            (TranscriptRole::AgentToolResult, Some(&coordinates), false),
            (TranscriptRole::UserMessage, None, false),
        ] {
            let key = transcript_fact_identity("thread", "message", 3, role, coordinates);
            assert_eq!(key.role, ManagedEventType::Transcript(role), "{role:?}");
            assert_eq!(
                key.provenance,
                if response {
                    FactProvenance::AssistantResponse {
                        run_id: "run".into(),
                        step: 2,
                        response: 1,
                    }
                } else {
                    FactProvenance::Message {
                        message_id: "message".into(),
                        ordinal: 3,
                    }
                },
                "R1/R2 {role:?}"
            );
        }
    }

    #[test]
    fn public_input_has_one_source_and_internal_feedback_is_never_customer_evidence() {
        // C1 customer-role transcript input; C2 exact Managed command already
        // owns its identity; C3 typed internal child/steer/continuation input;
        // C4 assistant/system role. R1 C1 without C2/C3 -> byte-faithful input;
        // R2 C2 -> existing receipt only; R3 C3/C4 -> no customer event.
        // A normal message containing internal-looking words remains public:
        // identity/role, never text or a language filter, selects these rules.
        let session = awaken_session_types::SessionId::from("session");
        let run_id = awaken_agent_contract::agent::run::Id("run".into());
        let runs = [RunRecord {
            id: run_id.clone(),
            thread_id: awaken_agent_contract::agent::thread::Id(session.as_str().into()),
            state: awaken_agent_contract::agent::run::RunState::Running,
        }];
        let text = "API 成本尚未提供，不能把它当成没有成本。";
        let ordinary = Message::text(MessageId("sdk-input".into()), Role::User, text);
        assert!(
            is_public_user_input(&ordinary, session.as_str(), &[], &runs),
            "R1 public input"
        );
        let batch = SessionEventBatch::compile(
            &session,
            "batch",
            vec![awaken_session_contract::SessionEventInput::UserMessage {
                content: ordinary.content.clone(),
            }],
        )
        .unwrap();
        let managed = Message::text(
            MessageId::session_event_input(session.as_str(), batch.events[0].event.operation_id()),
            Role::User,
            text,
        );
        assert!(
            !is_public_user_input(&managed, session.as_str(), &[batch], &runs),
            "R2 no duplicate receipt"
        );
        for input in [
            Message::text(MessageId::agent_thread_report(&run_id), Role::User, text),
            Message::text(MessageId::steer(&run_id, 1), Role::User, text),
            Message::text(MessageId::continuation(&run_id, 0, 1), Role::User, text),
            Message::text(MessageId("assistant".into()), Role::Assistant, text),
            Message::text(MessageId("system".into()), Role::System, text),
        ] {
            assert!(
                !is_public_user_input(&input, session.as_str(), &[], &runs),
                "R3 {:?}",
                input.id
            );
        }
    }
}
