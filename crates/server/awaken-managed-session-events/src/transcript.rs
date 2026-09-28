// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
//! Committed transcript evidence: assistant response coordinates, the exact
//! classification of every tool occurrence, and the neutral fold of Messages
//! into transcript facts. Shared by projection, ordering and reply admission.

use std::collections::HashMap;

use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::event::{Fact, ToolDisposition};
use awaken_session_application_contract::Pending;

/// The (Run, step, response) coordinate of every assistant Message owned by
/// one of `run_ids`, keyed by Message id. A response ordinal encoded in the
/// Message id is exact; a full-step response takes the next ordinal after
/// every encoded one seen before it in source order.
///
/// Consumers: the assistant fact identity shared with live previews, the
/// transcript fold, and the canonical order of those facts.
#[must_use]
pub fn assistant_response_coordinates(
    messages: &[Message],
    run_ids: &[RunId],
) -> HashMap<String, (String, usize, usize)> {
    let candidates: std::collections::HashSet<&str> =
        run_ids.iter().map(|run_id| run_id.0.as_str()).collect();
    let mut next_response = HashMap::<(String, usize), usize>::new();
    let mut coordinates = HashMap::new();
    for message in messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
    {
        let Some((run_id, step, response)) = message.id.assistant_coordinates() else {
            continue;
        };
        if !candidates.contains(run_id) {
            continue;
        }
        let next = next_response.entry((run_id.to_owned(), step)).or_default();
        let response = match response {
            Some(response) => {
                *next = (*next).max(response + 1);
                response
            }
            None => *next,
        };
        coordinates.insert(message.id.0.clone(), (run_id.to_owned(), step, response));
    }
    coordinates
}

/// Rebuild the exact Advisor ToolUse/ToolResult block coordinates from committed
/// transcript truth. Provider call ids are only batch-local, so the ordered fold
/// pairs each result with the corresponding call occurrence rather than globally
/// hiding every result that reuses the same raw id. A warm refresh may observe
/// the call before its result; classification therefore uses the full committed
/// prefix instead of a process-local hidden-call cursor.
#[derive(Default)]
pub struct AdvisorConsultationBlocks {
    calls: std::collections::HashSet<(String, usize)>,
    results: std::collections::HashSet<(String, usize)>,
}

impl AdvisorConsultationBlocks {
    /// Whether block `ordinal` of `message_id` is a consultation call.
    #[must_use]
    pub fn is_call(&self, message_id: &str, ordinal: usize) -> bool {
        self.calls.contains(&(message_id.to_string(), ordinal))
    }

    /// Whether block `ordinal` of `message_id` is a consultation result.
    #[must_use]
    pub fn is_result(&self, message_id: &str, ordinal: usize) -> bool {
        self.results.contains(&(message_id.to_string(), ordinal))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeToolProjectionPhase {
    Processed,
    PendingClient,
    PendingBuiltin,
    Resolving,
    Unclassified,
}

/// Server admission effect derived from the same phase that owns transcript
/// visibility. The queue variant delegates to the existing durable Session
/// Event selector; it is not a second scheduler or pending registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeToolFollowupAdmission {
    Reject,
    Queue,
    Ready,
}

impl RuntimeToolProjectionPhase {
    pub const fn from_pending(client_executed: bool) -> Self {
        if client_executed {
            Self::PendingClient
        } else {
            Self::PendingBuiltin
        }
    }

    pub const fn from_retained_reply(processed: bool) -> Self {
        if processed {
            Self::Processed
        } else {
            Self::Resolving
        }
    }

    pub const fn is_pending(self) -> bool {
        matches!(self, Self::PendingClient | Self::PendingBuiltin)
    }

    pub const fn followup_admission(self) -> RuntimeToolFollowupAdmission {
        match self {
            Self::PendingClient | Self::PendingBuiltin | Self::Unclassified => {
                RuntimeToolFollowupAdmission::Reject
            }
            Self::Resolving => RuntimeToolFollowupAdmission::Queue,
            Self::Processed => RuntimeToolFollowupAdmission::Ready,
        }
    }

    #[must_use]
    pub const fn is_transcript_visible(self) -> bool {
        !matches!(self, Self::Unclassified)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommittedToolOccurrence {
    message_id: String,
    block_ordinal: usize,
    call_id: String,
}

/// One read-only classification of a committed transcript prefix. This is the
/// shared root/child evidence seam: ToolResult pairs and the current ResumeTicket
/// classify exact ToolUse occurrences, while the existing Event log identifies
/// occurrences that were already emitted during an earlier partial refresh.
pub struct CommittedTranscriptEvidence {
    advisor_blocks: AdvisorConsultationBlocks,
    dispositions: std::collections::HashMap<CommittedToolOccurrence, RuntimeToolProjectionPhase>,
}

pub struct CommittedMessageProjection {
    pub message: awaken_agent_contract::agent::message::Message,
    pub fully_classified: bool,
    pub pending_occurrence: bool,
    pub retained_pending_occurrence: bool,
}

impl CommittedTranscriptEvidence {
    pub fn advisor_blocks(&self) -> &AdvisorConsultationBlocks {
        &self.advisor_blocks
    }

    /// Select only content whose committed disposition is known. Non-tool
    /// content stays in the source Message and is deduplicated by its existing
    /// stable public identity at append time. An unclassified ToolUse remains
    /// unconsumed; a previously emitted occurrence is derived from the existing
    /// source-qualified Event id rather than a second projection registry.
    pub fn project_message(
        &self,
        message: &awaken_agent_contract::agent::message::Message,
        projected_tool_sources: &std::collections::HashSet<(String, String)>,
        pending_source_id: Option<&str>,
        pending_history: &[&Pending],
    ) -> CommittedMessageProjection {
        use awaken_agent_contract::agent::content::ContentBlock;

        let mut projected = message.clone();
        let mut fully_classified = true;
        let mut pending_occurrence = false;
        let mut retained_pending_occurrence = false;
        projected.content = message
            .content
            .iter()
            .enumerate()
            .filter_map(|(block_ordinal, block)| {
                let ContentBlock::ToolUse { id, name, input } = block else {
                    return Some(block.clone());
                };
                if self
                    .advisor_blocks
                    .calls
                    .contains(&(message.id.0.clone(), block_ordinal))
                {
                    return Some(block.clone());
                }
                let key = CommittedToolOccurrence {
                    message_id: message.id.0.clone(),
                    block_ordinal,
                    call_id: id.clone(),
                };
                let mut disposition = self
                    .dispositions
                    .get(&key)
                    .copied()
                    .unwrap_or(RuntimeToolProjectionPhase::Unclassified);
                // A closed wait remains committed classification evidence even
                // when failure/cancellation removed its current ResumeTicket.
                // The caller has scoped history to this Thread/Run/message;
                // preserve unmatched sibling intents as Unclassified.
                if disposition == RuntimeToolProjectionPhase::Unclassified
                    && let Some(historical) = pending_history.iter().find(|pending| {
                        pending.tool_use_id == *id
                            && pending.name == *name
                            && pending.input == *input
                    })
                {
                    disposition =
                        RuntimeToolProjectionPhase::from_pending(historical.client_executed);
                }
                let is_pending = disposition.is_pending();
                pending_occurrence |= is_pending;
                let already_projected = projected_tool_sources
                    .contains(&(message.id.0.clone(), id.clone()))
                    || (is_pending
                        && pending_source_id.is_some_and(|source_id| {
                            projected_tool_sources.contains(&(source_id.to_string(), id.clone()))
                        }));
                if already_projected {
                    return None;
                }
                if !disposition.is_transcript_visible() {
                    fully_classified = false;
                    return None;
                }
                match disposition {
                    RuntimeToolProjectionPhase::PendingClient
                    | RuntimeToolProjectionPhase::PendingBuiltin => {
                        retained_pending_occurrence = true;
                        Some(block.clone())
                    }
                    RuntimeToolProjectionPhase::Processed
                    | RuntimeToolProjectionPhase::Resolving => Some(block.clone()),
                    RuntimeToolProjectionPhase::Unclassified => {
                        unreachable!("transcript visibility checked above")
                    }
                }
            })
            .collect();
        CommittedMessageProjection {
            message: projected,
            fully_classified,
            pending_occurrence,
            retained_pending_occurrence,
        }
    }
}

/// Fold the full committed prefix once into exact occurrence evidence. A result
/// consumes the earliest unmatched call with the same provider-local id; the
/// current ticket then classifies the earliest still-unmatched occurrence. This
/// ordered rule prevents raw-id reuse from turning an unrelated call into either
/// an executed or answerable Managed event.
pub fn committed_transcript_evidence(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
) -> CommittedTranscriptEvidence {
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Role;
    use awaken_runtime_contract::resolved::ADVISOR_TOOL_ID;

    struct Occurrence {
        key: CommittedToolOccurrence,
        advisor: bool,
        classification: RuntimeToolProjectionPhase,
    }

    let mut occurrences = Vec::<Occurrence>::new();
    let mut unmatched =
        std::collections::HashMap::<String, std::collections::VecDeque<usize>>::new();
    let mut advisor_blocks = AdvisorConsultationBlocks::default();
    for message in messages {
        if message.id.is_agent_thread_report() {
            continue;
        }
        match message.role {
            Role::Assistant => {
                for (block_ordinal, block) in message.content.iter().enumerate() {
                    let ContentBlock::ToolUse { id, name, .. } = block else {
                        continue;
                    };
                    let advisor = name == ADVISOR_TOOL_ID;
                    if advisor {
                        advisor_blocks
                            .calls
                            .insert((message.id.0.clone(), block_ordinal));
                    }
                    let index = occurrences.len();
                    occurrences.push(Occurrence {
                        key: CommittedToolOccurrence {
                            message_id: message.id.0.clone(),
                            block_ordinal,
                            call_id: id.clone(),
                        },
                        advisor,
                        classification: RuntimeToolProjectionPhase::Unclassified,
                    });
                    unmatched.entry(id.clone()).or_default().push_back(index);
                }
            }
            Role::Tool => {
                for (block_ordinal, block) in message.content.iter().enumerate() {
                    let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                        continue;
                    };
                    let Some(index) = unmatched
                        .get_mut(tool_use_id)
                        .and_then(std::collections::VecDeque::pop_front)
                    else {
                        continue;
                    };
                    if occurrences[index].advisor {
                        advisor_blocks
                            .results
                            .insert((message.id.0.clone(), block_ordinal));
                    } else {
                        occurrences[index].classification = RuntimeToolProjectionPhase::Processed;
                    }
                }
            }
            Role::User | Role::System => {}
        }
    }
    if let Some(pending) = pending
        && let Some(index) = unmatched
            .get(&pending.tool_use_id)
            .into_iter()
            .flatten()
            .copied()
            .find(|index| !occurrences[*index].advisor)
    {
        occurrences[index].classification =
            RuntimeToolProjectionPhase::from_pending(pending.client_executed);
    }
    let dispositions = occurrences
        .into_iter()
        .filter(|occurrence| !occurrence.advisor)
        .map(|occurrence| (occurrence.key, occurrence.classification))
        .collect();
    CommittedTranscriptEvidence {
        advisor_blocks,
        dispositions,
    }
}

#[cfg(kani)]
/// @obligation session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
/// @kani profile=solver-kissat-v1
#[kani::proof]
fn runtime_tool_projection_phase_is_total_distinct_and_fail_closed() {
    // Causes: C1 a current ticket is client/builtin; C2 an exact retained reply
    // exists; C3 that reply is processed. Effects: E1 !C2 stays Pending with
    // exact family; E2 C2+!C3 is Resolving; E3 C2+C3 is Processed; E4 missing
    // evidence is Unclassified, transcript-invisible, and rejected by server
    // admission. E5 Resolving durably queues behind the retained reply; E6
    // Processed is ready. All eight C1-C3 rows call the same production
    // constructors/predicates consumed by projection and admission; durable
    // identity matching, queue persistence and snapshot reads remain TCB.
    let client_executed: bool = kani::any();
    let retained: bool = kani::any();
    let processed: bool = kani::any();
    let phase = if retained {
        RuntimeToolProjectionPhase::from_retained_reply(processed)
    } else {
        RuntimeToolProjectionPhase::from_pending(client_executed)
    };

    assert_eq!(phase.is_pending(), !retained);
    assert_eq!(
        phase == RuntimeToolProjectionPhase::PendingClient,
        !retained && client_executed
    );
    assert_eq!(
        phase == RuntimeToolProjectionPhase::PendingBuiltin,
        !retained && !client_executed
    );
    assert_eq!(
        phase == RuntimeToolProjectionPhase::Resolving,
        retained && !processed
    );
    assert_eq!(
        phase == RuntimeToolProjectionPhase::Processed,
        retained && processed
    );
    assert!(phase.is_transcript_visible());
    assert!(!RuntimeToolProjectionPhase::Unclassified.is_transcript_visible());
    assert_eq!(
        phase.followup_admission(),
        if !retained {
            RuntimeToolFollowupAdmission::Reject
        } else if processed {
            RuntimeToolFollowupAdmission::Ready
        } else {
            RuntimeToolFollowupAdmission::Queue
        }
    );
    assert_eq!(
        RuntimeToolProjectionPhase::Unclassified.followup_admission(),
        RuntimeToolFollowupAdmission::Reject
    );
}

pub fn advisor_consultation_blocks(
    messages: &[awaken_agent_contract::agent::message::Message],
) -> AdvisorConsultationBlocks {
    committed_transcript_evidence(messages, None).advisor_blocks
}

/// Fold committed Messages into neutral transcript facts: the Runtime's own
/// fold, minus internal reports and Advisor consultation blocks, plus the
/// current ticket's call when no committed ToolUse carries it, with every call
/// whose committed Awaiting history names it restored to its pending family.
///
/// Consumer: the protocol adapter's transcoder, which encodes each fact on
/// its wire.
#[must_use]
pub fn fold_transcript(
    messages: &[Message],
    pending: Option<&Pending>,
    projected_tool_ids: &std::collections::HashSet<String>,
    advisor_blocks: &AdvisorConsultationBlocks,
    pending_history: &[&Pending],
) -> Vec<Fact> {
    let mut facts = fold_with_pending(messages, pending, projected_tool_ids, advisor_blocks);
    // The transcript establishes occurrence visibility; committed AwaitTarget
    // history establishes its permission/ownership at that occurrence. Preserve
    // every prior ask/client call even when a cold read sees all results at once.
    for fact in &mut facts {
        if let Fact::ToolCall {
            id,
            name,
            input,
            disposition,
        } = fact
            && let Some(pending) = pending_history.iter().find(|pending| {
                pending.tool_use_id == *id && pending.name == *name && pending.input == *input
            })
        {
            *disposition = if pending.client_executed {
                ToolDisposition::PendingClient
            } else {
                ToolDisposition::PendingBuiltin
            };
        }
    }
    facts
}

fn fold_with_pending(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
    projected_tool_ids: &std::collections::HashSet<String>,
    advisor_blocks: &AdvisorConsultationBlocks,
) -> Vec<Fact> {
    // Root continuations and Advisor consultations stay durable for Runtime
    // replay and Host reconstruction, but neither is an ordinary Managed
    // message/tool event. Filter both at this one message decoder instead of
    // teaching warm and cold callers separate suppression rules.
    let requires_filter = messages.iter().any(|message| {
        message.id.is_agent_thread_report()
            || message
                .content
                .iter()
                .enumerate()
                .any(|(ordinal, block)| match block {
                    awaken_agent_contract::agent::content::ContentBlock::ToolUse { .. } => {
                        advisor_blocks
                            .calls
                            .contains(&(message.id.0.clone(), ordinal))
                    }
                    awaken_agent_contract::agent::content::ContentBlock::ToolResult { .. } => {
                        advisor_blocks
                            .results
                            .contains(&(message.id.0.clone(), ordinal))
                    }
                    _ => false,
                })
    });
    let visible_messages;
    let messages = if requires_filter {
        visible_messages =
            messages
                .iter()
                .filter(|message| !message.id.is_agent_thread_report())
                .map(|message| {
                    let mut message = message.clone();
                    let message_id = message.id.0.clone();
                    message.content =
                        message
                            .content
                            .into_iter()
                            .enumerate()
                            .filter_map(|(ordinal, block)| {
                                match &block {
                        awaken_agent_contract::agent::content::ContentBlock::ToolUse { .. }
                            if advisor_blocks.calls.contains(&(message_id.clone(), ordinal)) =>
                        {
                            None
                        }
                        awaken_agent_contract::agent::content::ContentBlock::ToolResult {
                            ..
                        } if advisor_blocks
                            .results
                            .contains(&(message_id.clone(), ordinal)) => None,
                        _ => Some(block),
                    }
                            })
                            .collect();
                    message
                })
                .collect::<Vec<_>>();
        visible_messages.as_slice()
    } else {
        messages
    };
    let mut facts = awaken_agent_contract::event::fold_messages(
        messages,
        pending.map(|pending| (pending.tool_use_id.as_str(), pending.client_executed)),
    );
    // Advisor is a Managed orchestration primitive, not an ordinary public
    // tool. Retain the Runtime's committed ToolUse/ToolResult for model replay
    // and Host link reconstruction, but remove the pair only at this Managed
    // transcoder. The call is recognized by its reserved descriptor identity;
    // the later result is correlated to its exact message block by an ordered
    // fold of the full committed prefix.
    if let Some(pending) = pending
        && pending.name != awaken_runtime_contract::resolved::ADVISOR_TOOL_ID
        && !projected_tool_ids.contains(&pending.tool_use_id)
        && !facts
            .iter()
            .any(|fact| matches!(fact, Fact::ToolCall { id, .. } if id == &pending.tool_use_id))
    {
        facts.push(Fact::ToolCall {
            id: pending.tool_use_id.clone(),
            name: pending.name.clone(),
            input: pending.input.clone(),
            disposition: if pending.client_executed {
                ToolDisposition::PendingClient
            } else {
                ToolDisposition::PendingBuiltin
            },
        });
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;

    #[test]
    fn indexed_assistant_coordinates_match_ordered_response_rules() {
        // Causes: C1 selected/foreign Run; C2 Assistant/other role; C3 full,
        // partial, malformed identity; C4 duplicate or reordered responses;
        // C5 Run names overlap/contain delimiters; C6 selected Run repeated.
        // Rules A1=!C1 or !C2 or malformed => omitted without advancing;
        // A2=selected Assistant partial => exact encoded ordinal and next=max;
        // A3=selected Assistant full => current next, initially zero;
        // A4=duplicate message => last coordinate wins in source order;
        // A5=overlap/duplicate Runs => exact owner only, never double advance.
        // Exhaust all streams of length 0..=4 over these ten cause partitions.
        // The oracle states the retained prefix-scan behavior; it is test-only,
        // not a second projection path or persisted source of coordinates.
        fn oracle(
            messages: &[Message],
            runs: &[RunId],
        ) -> std::collections::HashMap<String, (String, usize, usize)> {
            let mut runs = runs.to_vec();
            runs.sort_by_key(|run| std::cmp::Reverse(run.0.len()));
            let mut next = std::collections::HashMap::<(String, usize), usize>::new();
            let mut result = std::collections::HashMap::new();
            for message in messages
                .iter()
                .filter(|message| message.role == Role::Assistant)
            {
                for run in &runs {
                    if let Some((step, response)) = message.id.assistant_truncated_response_of(run)
                    {
                        let value = next.entry((run.0.clone(), step)).or_default();
                        *value = (*value).max(response + 1);
                        result.insert(message.id.0.clone(), (run.0.clone(), step, response));
                        break;
                    }
                    if let Some(step) = message.id.assistant_step_of(run) {
                        result.insert(
                            message.id.0.clone(),
                            (
                                run.0.clone(),
                                step,
                                *next.entry((run.0.clone(), step)).or_default(),
                            ),
                        );
                        break;
                    }
                }
            }
            result
        }
        let root = RunId("root".into());
        let nested = RunId("root-assistant-0".into());
        let foreign = RunId("foreign".into());
        let alphabet = [
            (MessageId::assistant(&root, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 2), Role::Assistant),
            (MessageId::assistant(&root, 1), Role::Assistant),
            (MessageId::assistant(&nested, 0), Role::Assistant),
            (
                MessageId::assistant_truncated(&nested, 0, 1),
                Role::Assistant,
            ),
            (MessageId::assistant(&foreign, 0), Role::Assistant),
            (MessageId::assistant_truncated(&root, 0, 9), Role::User),
            (MessageId::assistant(&root, 0), Role::Tool),
            (MessageId("root-assistant-bad".into()), Role::Assistant),
        ]
        .map(|(id, role)| Message::text(id, role, "coordinate fixture"));
        let runs = [root.clone(), nested.clone(), root.clone()];
        for len in 0..=4_u32 {
            for code in 0..alphabet.len().pow(len) {
                let mut code = code;
                let messages: Vec<_> = (0..len)
                    .map(|_| {
                        let message = alphabet[code % alphabet.len()].clone();
                        code /= alphabet.len();
                        message
                    })
                    .collect();
                assert_eq!(
                    assistant_response_coordinates(&messages, &runs),
                    oracle(&messages, &runs)
                );
            }
        }
        let messages = alphabet.to_vec();
        assert!(
            assistant_response_coordinates(&messages, &[]).is_empty(),
            "A1 no selected Runs"
        );
        let expected = oracle(&messages, &runs);
        for count in [0, 10, 10_000] {
            let mut expanded: Vec<_> = (0..count)
                .map(|i| RunId(format!("unrelated-{i}")))
                .collect();
            expanded.extend(runs.clone());
            assert_eq!(
                assistant_response_coordinates(&messages, &expanded),
                expected,
                "A5 unrelated Run cardinality {count}"
            );
        }
    }

    #[test]
    fn runtime_tool_projection_phase_decision_table() {
        // Causes: C1 current ticket is builtin/client; C2 an exact retained
        // reply exists; C3 its root entry is processed. Effects: P1 !C2 stays
        // Pending in the exact family and blocks follow-up; P2 C2+!C3 is
        // Resolving; P3 C2+C3 is Processed; P4 missing disposition evidence is
        // Unclassified, withheld and rejected. P5 Resolving queues through the
        // durable selector; P6 Processed is ready. Rules cover all C1-C3
        // combinations; the exact identity adapter and production queue effect
        // are exercised by Managed event recovery tests.
        for client_executed in [false, true] {
            for retained in [false, true] {
                for processed in [false, true] {
                    let phase = if retained {
                        RuntimeToolProjectionPhase::from_retained_reply(processed)
                    } else {
                        RuntimeToolProjectionPhase::from_pending(client_executed)
                    };
                    assert_eq!(phase.is_pending(), !retained, "P1-P3");
                    assert_eq!(
                        phase == RuntimeToolProjectionPhase::Resolving,
                        retained && !processed,
                        "P2"
                    );
                    assert_eq!(
                        phase == RuntimeToolProjectionPhase::Processed,
                        retained && processed,
                        "P3"
                    );
                    assert!(phase.is_transcript_visible(), "P1-P3");
                    assert_eq!(
                        phase.followup_admission(),
                        if !retained {
                            RuntimeToolFollowupAdmission::Reject
                        } else if processed {
                            RuntimeToolFollowupAdmission::Ready
                        } else {
                            RuntimeToolFollowupAdmission::Queue
                        },
                        "P1/P5/P6"
                    );
                }
            }
        }
        assert!(
            !RuntimeToolProjectionPhase::Unclassified.is_transcript_visible(),
            "P4"
        );
        assert_eq!(
            RuntimeToolProjectionPhase::Unclassified.followup_admission(),
            RuntimeToolFollowupAdmission::Reject,
            "P4"
        );
    }
}
