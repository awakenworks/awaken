// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: One committed auxiliary Run is the sole proposal source. The original Memory
// @formal reason: controller owns intent/claim/receipt transitions and the repository owns content CAS.
// @formal reason: Exact input, Run/Thread identity, successful settlement and successful tool results
// @formal reason: are checked before any proposal map escapes; malformed batches produce no writes.
//! Memory proposals carried by an ordinary extractor Run, without a content store or executor.

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_memory::{
    EXTRACT_PROMPT, MemoryExtractionIntent, MemoryExtractionOutcome, MemoryStoreHandle,
    WriteMemoryTool, accepts_memory_content, sanitize_stem,
};
use awaken_runtime_contract::{
    ExecutableAgentSnapshot,
    resolved::{ToolDescriptor, ToolKind},
};
use awaken_tool_contract::tool::Tool;

/// A write proposal has no immediate content effect. The existing tool records its
/// result in the Run; only the extraction controller later admits durable mutations.
#[derive(Default)]
pub struct MemoryWriteProposals;

impl MemoryWriteProposals {
    /// Select the frozen extraction contract, rather than a same-named Session
    /// store write or client-owned tool. The descriptor's existing namespace
    /// owns that distinction; the live tool still owns argument validation.
    #[must_use]
    pub fn descriptor(snapshot: &ExecutableAgentSnapshot) -> Option<&ToolDescriptor> {
        let canonical = awaken_ext_memory::write_memory_descriptor();
        snapshot
            .resolved_spec
            .tool_descriptors
            .iter()
            .find(|descriptor| {
                descriptor.id == canonical.id
                    && descriptor.content_namespace() == canonical.content_namespace()
                    && descriptor.kind == ToolKind::Regular
                    && descriptor.provider_server_tool.is_none()
            })
    }
}

#[async_trait]
impl MemoryStoreHandle for MemoryWriteProposals {
    async fn write(&self, name: &str, _content: &str) -> Result<String, String> {
        Ok(format!("/{}.md", sanitize_stem(name)))
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        Ok(Vec::new())
    }
}

/// The unchanged extractor request, shared by embedded execution and dispatch.
#[must_use]
pub fn memory_extraction_input(intent: &MemoryExtractionIntent) -> Vec<Message> {
    let mut input = intent.transcript.clone();
    input.push(Message::text(
        MessageId(format!("{}-mem-prompt", intent.terminal_commit_id)),
        Role::User,
        intent
            .extractor
            .extraction_prompt
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(EXTRACT_PROMPT),
    ));
    input
}

/// Refusal to interpret a committed auxiliary Run as a complete Memory proposal batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MemoryProposalError {
    #[error("Memory extraction has no known completed auxiliary Run")]
    ExecutionIndeterminate,
    #[error("Memory extraction auxiliary Run belongs to another Run or Thread")]
    RunIdentityMismatch,
    #[error("Memory extraction auxiliary Run did not end successfully")]
    UnsuccessfulRun,
    #[error("Memory extraction transcript does not retain its exact admitted input")]
    InputMismatch,
    #[error("Memory extraction has no committed assistant output for its auxiliary Run")]
    MissingOutput,
    #[error("Memory extraction contains an undeclared, malformed or repeated write_memory call")]
    InvalidWrite,
    #[error("Memory extraction write_memory call has no unique successful committed result")]
    UncommittedWrite,
}

impl MemoryProposalError {
    /// Unknown execution retains the original Run identity; corrupt known output
    /// is a permanent refusal, never a retry that starts another business effect.
    #[must_use]
    pub fn into_extraction_outcome(
        self,
        intent: &MemoryExtractionIntent,
    ) -> MemoryExtractionOutcome {
        let reason = self.to_string();
        if self == Self::ExecutionIndeterminate {
            MemoryExtractionOutcome::ExecutionIndeterminate {
                auxiliary_run_id: intent.auxiliary_run_id(),
                reason,
            }
        } else {
            MemoryExtractionOutcome::PermanentFailure { reason }
        }
    }
}

/// Decode only the outputs of this exact, successfully settled extractor Run.
/// The caller supplies the record and messages from one committed recovery
/// prefix; the complete batch is verified before it reaches mutation planning.
pub fn committed_memory_writes(
    intent: &MemoryExtractionIntent,
    record: Option<&RunRecord>,
    messages: &[Message],
) -> Result<BTreeMap<String, String>, MemoryProposalError> {
    let run = RunId(intent.auxiliary_run_id());
    let thread = ThreadId(intent.auxiliary_thread_id());
    let record = record.ok_or(MemoryProposalError::ExecutionIndeterminate)?;
    if record.id != run || record.thread_id != thread {
        return Err(MemoryProposalError::RunIdentityMismatch);
    }
    match record.state {
        RunState::Ended(EndCause::NaturalEnd | EndCause::MaxSteps) => {}
        RunState::Running | RunState::Ended(EndCause::Indeterminate) => {
            return Err(MemoryProposalError::ExecutionIndeterminate);
        }
        _ => return Err(MemoryProposalError::UnsuccessfulRun),
    }
    let input = memory_extraction_input(intent);
    let outputs = messages
        .strip_prefix(input.as_slice())
        .ok_or(MemoryProposalError::InputMismatch)?;
    let mut writes = BTreeMap::new();
    let mut calls = HashSet::new();
    let mut observed_output = false;
    for (index, message) in outputs.iter().enumerate() {
        if message.role != Role::Assistant {
            continue;
        }
        let Some(step) = message.id.assistant_step_of(&run) else {
            continue;
        };
        observed_output = true;
        for block in &message.content {
            let ContentBlock::ToolUse { id, name, input } = block else {
                continue;
            };
            if name != WriteMemoryTool::ID {
                continue;
            }
            if MemoryWriteProposals::descriptor(&intent.extractor.agent).is_none() {
                return Err(MemoryProposalError::InvalidWrite);
            }
            if !calls.insert((step, id.as_str())) {
                return Err(MemoryProposalError::InvalidWrite);
            }
            let args: awaken_ext_memory::tool::WriteMemoryArgs =
                serde_json::from_value(input.clone())
                    .map_err(|_| MemoryProposalError::InvalidWrite)?;
            if !accepts_memory_content(&args.content) {
                continue;
            }
            let mut results = outputs[index + 1..]
                .iter()
                .filter(|result| {
                    result.role == Role::Tool && result_matches(&result.id, &run, step, id)
                })
                .flat_map(|result| &result.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } if tool_use_id == id => Some(*is_error),
                    _ => None,
                });
            if results.next() != Some(false) || results.next().is_some() {
                return Err(MemoryProposalError::UncommittedWrite);
            }
            writes.insert(format!("/{}.md", sanitize_stem(&args.name)), args.content);
        }
    }
    if !observed_output {
        return Err(MemoryProposalError::MissingOutput);
    }
    Ok(writes)
}

fn result_matches(id: &MessageId, run: &RunId, step: usize, call: &str) -> bool {
    match id.tool_result_coordinates() {
        Some((owner, owner_step, owner_call)) => {
            owner == run.0 && owner_step == step && owner_call == call
        }
        None => id == &MessageId::tool_result(call),
    }
}

#[cfg(test)]
mod tests;
