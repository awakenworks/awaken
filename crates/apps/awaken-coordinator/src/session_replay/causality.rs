use std::collections::{BTreeMap, BTreeSet};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::read::replay::CommittedThreadPrefix;
use awaken_agent_contract::{RunId, ThreadId};
use awaken_ext_builtin_tools::{
    AgentMessageReceipt, AgentMessageTarget, SEND_MESSAGE, SendMessageArgs,
};
use awaken_session_application::replay::{
    ReplayAgentMessage, ReplayAgentMessageSource, ReplayRunActivation, SessionReplayError,
};
use awaken_session_execution_contract::{
    agent_report_run_id, coordinated_activation_input, coordinated_run_id, coordinated_thread_id,
    session_agent_report_text,
};
use awaken_session_types::SessionId;
use awaken_tool_contract::{
    tool::ToolOutput,
    tool_batch::{ActiveToolBatch, ToolCallPhase},
};

use super::history;

struct Send {
    source: RunId,
    call: String,
    operation: String,
    args: SendMessageArgs,
    requested: u64,
    executing: Option<u64>,
    receipt: Option<(u64, ThreadId)>,
}

fn committed_sends(root: &CommittedThreadPrefix) -> Result<Vec<Send>, SessionReplayError> {
    let mut sends = BTreeMap::new();
    let commands: Vec<_> = root
        .state
        .iter()
        .map(|(_, command)| command.clone())
        .collect();
    let runs: BTreeSet<_> = root.runs.iter().map(|(_, run, _)| run).collect();
    for run in runs {
        for record in ActiveToolBatch::recorded_batches(&commands, run) {
            let (position, batch) = record.map_err(history)?;
            let Some(batch) = batch else { continue };
            let sequence = root.state[position].0;
            for entry in batch
                .calls()
                .iter()
                .filter(|entry| entry.call.tool_id == SEND_MESSAGE)
            {
                let operation = batch.operation_id(&entry.call.call_id);
                let key = (run.clone(), operation.clone());
                // Invalid model requests may exist in history and have no accepted edge.
                let Ok(args) =
                    serde_json::from_value::<SendMessageArgs>(entry.call.arguments.clone())
                else {
                    continue;
                };
                if args.normalized_target().is_err() || args.message.trim().is_empty() {
                    continue;
                }
                let send = sends.entry(key).or_insert_with(|| Send {
                    source: run.clone(),
                    call: entry.call.call_id.clone(),
                    operation,
                    args,
                    requested: sequence,
                    executing: None,
                    receipt: None,
                });
                if matches!(entry.phase, ToolCallPhase::Executing { .. })
                    && send.executing.is_none()
                {
                    send.executing = Some(sequence);
                }
                let output = match &entry.phase {
                    ToolCallPhase::Returned(output)
                    | ToolCallPhase::Completed(output)
                    | ToolCallPhase::KnownAborted { output, .. } => Some(output),
                    _ => None,
                };
                if send.receipt.is_none()
                    && let Some(output) = output.filter(|output| output.call_id == send.call)
                    && let Some(thread) = accepted_thread(output)?
                {
                    send.receipt = Some((sequence, thread));
                }
            }
        }
    }
    let mut sends: Vec<_> = sends.into_values().collect();
    sends.sort_by_key(|send| send.requested);
    Ok(sends)
}

fn accepted_thread(output: &ToolOutput) -> Result<Option<ThreadId>, SessionReplayError> {
    if output.is_error {
        return Ok(None);
    }
    let text: String = output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let receipt: AgentMessageReceipt = serde_json::from_str(&text).map_err(history)?;
    if !receipt.accepted || receipt.session_thread_id.is_empty() {
        return Err(history("invalid committed Agent message receipt"));
    }
    Ok(Some(ThreadId(receipt.session_thread_id)))
}

pub(super) fn project_sends(
    session: &SessionId,
    root: &CommittedThreadPrefix,
    archives: &[ReplayRunActivation],
) -> Result<(Vec<ThreadId>, Vec<ReplayAgentMessage>), SessionReplayError> {
    let mut children = BTreeSet::new();
    let mut edges = Vec::new();
    let by_run: BTreeMap<_, _> = archives
        .iter()
        .map(|archive| (&archive.configuration.activation.run_id, archive))
        .collect();
    for send in committed_sends(root)? {
        let target = send.args.normalized_target().map_err(history)?;
        let thread = match &target {
            AgentMessageTarget::Spawn { .. } => {
                coordinated_thread_id(session, &send.source, &send.operation)
            }
            AgentMessageTarget::ExistingThread { session_thread_id } => {
                ThreadId(session_thread_id.clone())
            }
        };
        let run = coordinated_run_id(session, &thread, &send.source, &send.operation);
        let receipt_at = send
            .receipt
            .map(|(sequence, received)| {
                if received != thread {
                    return Err(history("Agent receipt differs from its committed target"));
                }
                Ok(sequence)
            })
            .transpose()?;
        let first = if let Some(archive) = by_run.get(&run) {
            let activation = &archive.configuration.activation;
            let input = coordinated_activation_input(
                session.as_str(),
                &thread,
                &run,
                &send.operation,
                send.args.message,
            );
            if activation.thread_id != thread
                || activation.input != input
                || send
                    .executing
                    .is_none_or(|sequence| sequence >= archive.first_commit)
                || activation
                    .delegation_origin
                    .as_ref()
                    .is_none_or(|recorded| {
                        let [ancestor] = recorded.agent_lineage.as_slice() else {
                            return true;
                        };
                        ancestor.is_empty()
                            || *recorded
                                != awaken_dispatch_execution::coordinated_run_origin(
                                    session.as_str(),
                                    &send.operation,
                                    send.source.clone(),
                                    send.call.clone(),
                                    ancestor.clone(),
                                )
                    })
                || matches!(&target, AgentMessageTarget::Spawn { agent_id } if activation.snapshot.root_agent_id.0 != *agent_id)
            {
                return Err(history(
                    "child activation does not match its committed source operation",
                ));
            }
            Some(archive.first_commit)
        } else {
            None
        };
        if matches!(target, AgentMessageTarget::Spawn { .. })
            && (receipt_at.is_some() || first.is_some())
        {
            children.insert(thread.0.clone());
        }
        edges.push(ReplayAgentMessage {
            source_thread: root.thread_id.clone(),
            source_run: send.source,
            target_thread: thread,
            target_run: run,
            source: ReplayAgentMessageSource::Send {
                call_id: send.call,
                operation_id: send.operation,
                requested_at: send.requested,
                receipt_at,
            },
            target_first_commit: first,
            target_input_commit: None,
        });
    }
    Ok((children.into_iter().map(ThreadId).collect(), edges))
}

pub(super) fn complete_messages(
    session: &SessionId,
    threads: &[CommittedThreadPrefix],
    archives: &[ReplayRunActivation],
    edges: &mut Vec<ReplayAgentMessage>,
) -> Result<(), SessionReplayError> {
    let Some((root, children)) = threads.split_first() else {
        return Ok(());
    };
    for edge in &mut *edges {
        let Some(thread) = children
            .iter()
            .find(|thread| thread.thread_id == edge.target_thread)
        else {
            continue;
        };
        let ReplayAgentMessageSource::Send { operation_id, .. } = &edge.source else {
            continue;
        };
        let input = coordinated_activation_input(
            session.as_str(),
            &edge.target_thread,
            &edge.target_run,
            operation_id,
            String::new(),
        );
        let first_commit = thread
            .runs
            .iter()
            .find(|(_, run, _)| run == &edge.target_run)
            .map(|(sequence, _, _)| *sequence);
        if edge.target_first_commit.is_some() && edge.target_first_commit != first_commit {
            return Err(history(
                "archived activation does not match the first Thread commit",
            ));
        }
        edge.target_first_commit = first_commit;
        edge.target_input_commit = input_commit(thread, &input[0].id, edge.target_first_commit);
    }
    for child in children {
        let mut reported_runs = BTreeSet::new();
        let messages: Vec<_> = child
            .messages
            .iter()
            .map(|(_, message)| message.clone())
            .collect();
        for (completed_at, run, state) in &child.runs {
            if !matches!(state, RunState::Ended(EndCause::NaturalEnd)) || !reported_runs.insert(run)
            {
                continue;
            }
            let report_run = agent_report_run_id(session, run);
            let report_id = MessageId::agent_thread_report(run);
            let first = root
                .runs
                .iter()
                .find(|(_, run, _)| run == &report_run)
                .map(|(sequence, _, _)| *sequence);
            let Some(first) = first else {
                continue;
            };
            let consumed = input_commit(root, &report_id, Some(first));
            let frozen_report = archives.iter().any(|archive| {
                let activation = &archive.configuration.activation;
                activation.run_id == report_run
                    && activation.thread_id == root.thread_id
                    && activation.input.len() == 1
                    && activation.input[0].id == report_id
                    && activation.input[0].role == Role::User
            });
            if *completed_at >= first
                || (!frozen_report && consumed.is_none())
                || session_agent_report_text(&messages, run).is_empty()
            {
                return Err(history(
                    "report continuation lacks exact committed child provenance",
                ));
            }
            edges.push(ReplayAgentMessage {
                source_thread: child.thread_id.clone(),
                source_run: run.clone(),
                target_thread: root.thread_id.clone(),
                target_run: report_run,
                source: ReplayAgentMessageSource::Report {
                    completed_at: *completed_at,
                },
                target_first_commit: Some(first),
                target_input_commit: consumed,
            });
        }
    }
    Ok(())
}

fn input_commit(thread: &CommittedThreadPrefix, id: &MessageId, first: Option<u64>) -> Option<u64> {
    let first = first?;
    thread
        .messages
        .iter()
        .find(|(sequence, message)| {
            *sequence >= first && message.id == *id && message.role == Role::User
        })
        .map(|(sequence, _)| *sequence)
}
