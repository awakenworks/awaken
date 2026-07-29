//! Agent-domain contract for runtime truth, commits, facts, and neutral events.

pub mod agent;
pub mod audit;
pub mod event;
pub mod fingerprint;
pub mod model_spec;
pub mod page;
pub mod secret;
pub mod stream;
pub mod thread;
pub mod toolset;

pub use agent::delegation::{DelegationId, DelegationOrigin};
pub use agent::message::Message;
pub use agent::run::{Id as RunId, Record as RunRecord};
pub use agent::state::Key as StateKey;
pub use agent::thread::Id as ThreadId;
pub use audit::record::Record as EventRecord;
pub use event::AgentEvent;
pub use fingerprint::stable_fingerprint;
pub use model_spec::ModelSpec;
pub use secret::{RedactedString, StructuredCredentialMaterial};
pub use stream::event::Event as StreamEvent;
pub use stream::sink::Sink as StreamSink;
pub use thread::commit::coordinator::Coordinator as CommitCoordinator;
pub use thread::commit::run_fact::RunFact;
pub use thread::commit::staged::ThreadCommit;
pub use thread::read::checkpoint::{CheckpointReader, EventScope};
pub use thread::read::lifecycle::{
    CheckpointRunLifecycleFeed, LifecycleCursor, LifecyclePage, RunLifecycleEvent,
    RunLifecycleFeed, RunLifecycleFeedError, RunLifecycleKind, classify_run_lifecycle,
};
pub use thread::read::transcript::{
    TranscriptError, TranscriptRange, TranscriptSlice, TranscriptSliceSpec, TranscriptSnapshot,
    TranscriptSnapshotRef, TranscriptView,
};
pub use toolset::{
    ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
    ToolsetSource,
};
