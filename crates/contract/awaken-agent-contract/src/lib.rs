// @formal entrypoint tool_policy.selector_fails_closed_for_unregistered_widened_or_mismatched_calls
//! Agent-domain contract for runtime truth, commits, facts, and neutral events.
//!
//! # Boundary
//!
//! Context: `agent-runtime`.
//!
//! Published SDK contract (ADR-0086): every context may read it, so its readers are not listed
//! here; `cargo tree -e normal -i awaken-agent-contract` prints them.

pub mod acp;
pub mod agent;
pub mod audit;
pub mod credential;
pub mod event;
pub mod fingerprint;
pub mod id;
pub mod mcp_target;
pub mod mcp_tool_id;
pub mod model_spec;
pub mod model_target;
pub mod observation;
pub mod page;
pub mod resilience;
pub mod scope;
pub mod secret;
pub mod skill;
pub mod stream;
pub mod thread;
pub mod toolset;

pub use acp::AcpSessionConfiguration;
pub use agent::delegation::{DelegationId, DelegationOrigin};
pub use agent::message::Message;
pub use agent::run::{Id as RunId, Record as RunRecord};
pub use agent::state::Key as StateKey;
pub use agent::thread::Id as ThreadId;
pub use audit::record::Record as EventRecord;
pub use event::AgentEvent;
pub use fingerprint::{
    CanonicalJsonHashError, canonical_json_sha256, collision_resistant_fingerprint,
    collision_resistant_fingerprint_digest, content_fingerprint, stable_fingerprint,
};
pub use id::fresh_process_id;
pub use mcp_target::{
    HttpMcpTarget, McpTarget, McpTargetError, McpTargetIdentity, SandboxStdioMcpTarget,
    SandboxStdioMcpTargetKind,
};
pub use mcp_tool_id::{
    MCP_DOTTED_TOOL_PREFIX, MCP_TOOL_ID_PREFIX, MCP_TOOL_ID_SEPARATOR, McpToolId,
    McpToolIdComponent, McpToolIdError,
};
pub use model_spec::ModelSpec;
pub use model_target::ModelTarget;
pub use observation::{Observed, UnknownCause};
pub use scope::{ExecutionScopeRef, ScopeId};
pub use secret::{RedactedString, RedactedStringSource, StructuredCredentialMaterial};
pub use skill::{ANTHROPIC_SKILL_IDS, AgentSkillBinding, AgentSkillKind, validate_agent_skills};
pub use stream::event::Event as StreamEvent;
pub use stream::sink::Sink as StreamSink;
pub use thread::commit::coordinator::Coordinator as CommitCoordinator;
pub use thread::commit::run_fact::RunFact;
pub use thread::commit::staged::ThreadCommit;
pub use thread::disposition::{
    ThreadDisposition, archive_thread_command, archived_thread_commit_cursor,
    thread_disposition_from_committed_state,
};
pub use thread::read::checkpoint::{CheckpointReader, EventScope};
pub use thread::read::lifecycle::{
    CheckpointRunLifecycleFeed, RunLifecycleCursor, RunLifecycleCursorCodecError,
    RunLifecycleEvent, RunLifecycleEventKind, RunLifecycleFeed, RunLifecycleFeedError,
    RunLifecyclePage, classify_run_lifecycle_event, classify_run_lifecycle_record,
    decode_run_lifecycle_cursor, encode_run_lifecycle_cursor,
};
pub use thread::read::transcript::{
    TranscriptError, TranscriptRange, TranscriptSlice, TranscriptSliceSpec, TranscriptSnapshot,
    TranscriptSnapshotRef, TranscriptView,
};
pub use toolset::{
    ClientToolDescriptor, ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride,
    ToolsetPolicy, ToolsetSource, select_tool_policy,
};
