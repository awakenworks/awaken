//! Committed audit and state a fixture Runtime exposes, written as a
//! Runtime's append-only commits would record them.

/// The await target each audited Awaiting commit recorded, by its cursor.
pub type AwaitTargets = std::collections::HashMap<
    awaken_agent_contract::RunLifecycleCursor,
    awaken_agent_contract::agent::awaiting::AwaitTarget,
>;

/// Record the await target a Runtime commit writes into the audit of the
/// Awaiting transition that opens `ticket`: the Thread's latest visible
/// lifecycle commit. A fixture exposes its tickets as they stand now, so the
/// target is remembered for every later snapshot, after the ticket is
/// consumed, as the immutable audit row is. An `unaudited` Awaiting models a
/// legacy commit written before the target was recorded.
pub fn remember_ticket_await_target<'a>(
    thread_lifecycle: impl IntoIterator<Item = &'a awaken_agent_contract::RunLifecycleEvent>,
    ticket: Option<&awaken_agent_contract::agent::awaiting::AwaitTarget>,
    unaudited: &std::collections::HashSet<awaken_agent_contract::RunLifecycleCursor>,
    targets: &mut AwaitTargets,
) {
    let (Some(ticket), Some(latest)) = (ticket, thread_lifecycle.into_iter().last()) else {
        return;
    };
    if latest.kind == awaken_agent_contract::RunLifecycleEventKind::Awaiting
        && !unaudited.contains(&latest.cursor)
    {
        targets
            .entry(latest.cursor)
            .or_insert_with(|| ticket.clone());
    }
}

/// The committed state command that records one Thread's usage.
pub fn fixture_usage_command(
    usage: awaken_session_budget_contract::SessionUsage,
) -> awaken_agent_contract::agent::state::Command {
    let mut attributed = awaken_runtime_contract::llm::ThreadUsage::default();
    let token_usage = |input, output, read, creation| awaken_runtime_contract::llm::TokenUsage {
        prompt_tokens: input,
        completion_tokens: output,
        cache_read_tokens: read,
        cache_creation_tokens: creation,
    };
    if usage.by_model.is_empty() {
        attributed.record(
            "test-fixture",
            token_usage(
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
            ),
        );
    } else {
        for (model, model_usage) in usage.by_model {
            attributed.record(
                &model,
                token_usage(
                    model_usage.input_tokens,
                    model_usage.output_tokens,
                    model_usage.cache_read_tokens,
                    model_usage.cache_creation_tokens,
                ),
            );
        }
    }
    <awaken_runtime_contract::llm::ThreadUsageKey as awaken_agent_contract::agent::state::StateKey>::write(&attributed)
}

/// One Thread's committed state commands, each with the commit that first
/// exposed it.
pub type StateLog = Vec<(awaken_agent_contract::agent::state::Command, u64)>;

/// Append every command of `current` not yet in `log` at `cursor`, then return
/// the whole log with its coordinates. A fixture that exposes only its current
/// state thereby commits each command once, at the first snapshot that shows
/// it, and keeps it afterwards, as a Runtime's append-only state does.
pub fn fixture_state_log(
    current: Vec<awaken_agent_contract::agent::state::Command>,
    cursor: u64,
    log: &mut StateLog,
) -> (Vec<awaken_agent_contract::agent::state::Command>, Vec<u64>) {
    let mut unmatched = log.iter().map(|_| true).collect::<Vec<_>>();
    for command in current {
        match log
            .iter()
            .enumerate()
            .position(|(index, (logged, _))| unmatched[index] && logged == &command)
        {
            Some(index) => unmatched[index] = false,
            None => {
                log.push((command, cursor));
                unmatched.push(false);
            }
        }
    }
    log.iter().cloned().unzip()
}

/// The committed audit of one Thread's visible lifecycle, with the target of
/// every pause the fixture's live `ticket` opened remembered and restored.
pub fn fixture_thread_audit(
    lifecycle: &[awaken_agent_contract::RunLifecycleEvent],
    thread_id: &str,
    store_cursor: u64,
    ticket: Option<&awaken_agent_contract::agent::awaiting::AwaitTarget>,
    unaudited: &std::collections::HashSet<awaken_agent_contract::RunLifecycleCursor>,
    targets: &mut AwaitTargets,
) -> Vec<awaken_agent_contract::audit::record::Record> {
    let visible = || {
        lifecycle.iter().filter(move |event| {
            event.thread_id.0 == thread_id && event.source_commit_cursor <= store_cursor
        })
    };
    remember_ticket_await_target(visible(), ticket, unaudited, targets);
    audited_lifecycle_records(visible(), targets)
}

/// The target a Runtime tool batch records when it pauses on `pending`.
pub fn tool_call_target(
    pending: awaken_session_application_contract::Pending,
    reason: awaken_agent_contract::agent::awaiting::ToolAwaitReason,
) -> awaken_agent_contract::agent::awaiting::AwaitTarget {
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, PendingTool, ToolCallAuthority};
    AwaitTarget::ToolCall {
        reason,
        call_id: pending.tool_use_id,
        tool: PendingTool {
            tool_id: pending.name,
            arguments: pending.input,
        },
        authority: ToolCallAuthority::RuntimeToolBatch,
    }
}

/// The audit of `lifecycle` when every Awaiting commit in it opened a ticket
/// for `target`, as the Runtime writes it into that commit's audit.
pub fn lifecycle_audit_pausing_on(
    lifecycle: &[awaken_agent_contract::RunLifecycleEvent],
    target: &awaken_agent_contract::agent::awaiting::AwaitTarget,
) -> Vec<awaken_agent_contract::audit::record::Record> {
    let targets = lifecycle
        .iter()
        .filter(|event| event.kind == awaken_agent_contract::RunLifecycleEventKind::Awaiting)
        .map(|event| (event.cursor, target.clone()))
        .collect();
    audited_lifecycle_records(lifecycle, &targets)
}

/// [`lifecycle_audit_records`] with the await target a Runtime commit records
/// beside an Awaiting transition that opened a ticket, as production writes it
/// in the same commit as the ticket.
pub fn audited_lifecycle_records<'a>(
    events: impl IntoIterator<Item = &'a awaken_agent_contract::RunLifecycleEvent>,
    targets: &AwaitTargets,
) -> Vec<awaken_agent_contract::audit::record::Record> {
    let mut records = lifecycle_audit_records(events);
    for record in &mut records {
        if let Some(target) =
            targets.get(&awaken_agent_contract::RunLifecycleCursor(record.sequence))
        {
            record.payload["await_target"] =
                serde_json::to_value(target).expect("await targets serialize");
        }
    }
    records
}

/// Materialize authored fixture transitions as real committed audit records.
/// Snapshot and lifecycle assertions must describe one fact set. Classification
/// still belongs to the production reducer, so an inconsistent expected kind
/// cannot manufacture an Awaiting predecessor or override the encoded state.
pub fn lifecycle_audit_records<'a>(
    events: impl IntoIterator<Item = &'a awaken_agent_contract::RunLifecycleEvent>,
) -> Vec<awaken_agent_contract::audit::record::Record> {
    use awaken_agent_contract::{RunLifecycleEventKind, audit::run_event::RunEvent};
    let mut records = events
        .into_iter()
        .map(|event| {
            let fact = if event.kind == RunLifecycleEventKind::Rescheduled {
                RunEvent::RunRescheduled {
                    state: event.state.clone(),
                    claim_epoch: 1,
                }
            } else {
                RunEvent::RunStateChanged {
                    state: event.state.clone(),
                    await_reason: event.await_reason,
                    await_target: None,
                }
            };
            let draft: awaken_agent_contract::audit::draft::Draft = fact.into();
            awaken_agent_contract::audit::record::Record {
                sequence: event.cursor.0,
                run_id: event.run_id.clone(),
                kind: draft.kind,
                payload: draft.payload,
            }
        })
        .collect::<Vec<_>>();
    records.sort_by_key(|record| record.sequence);
    records
}
