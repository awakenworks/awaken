//! Explicit Awaken HTTP extensions layered beside, never inside, the Anthropic
//! Managed Agents compatibility adapter.
//!
//! Every route is namespaced under `/v1/awaken`. This crate drives neutral
//! application ports; concrete application services are injected only by the
//! composition root.

mod dream_policies;
mod event_admissions;
mod execution_evidence;
mod file_outputs;
mod live_inbox;
mod memory_copy_conflicts;
mod profiled_sessions;
mod repositories;
mod resource_manifests;
mod sandbox_policies;

pub use dream_policies::dream_policy_router;
pub use event_admissions::{
    EventAdmissionProof, EventAdmissionReceipt, EventAdmissionStatus,
    session_event_admission_router,
};
pub use execution_evidence::{
    SessionExecutionEvidence, SessionModelRequestEvidence, SessionRunExecutionEvidence,
    session_execution_evidence_router,
};
pub use file_outputs::{
    FileOutputCapture, FileOutputCaptureParams, FileOutputCaptureResponse,
    file_output_capture_router,
};
pub use live_inbox::live_inbox_router;
pub use memory_copy_conflicts::memory_copy_conflicts_router;
pub use profiled_sessions::{
    ProfiledSessionCreate, ProfiledSessionCreated, ProfiledSessionMcpAttachment,
    ProfiledSessionMode, ProfiledSessionRelease, ProfiledSessionReleased,
    ProfiledSessionRepository, ProfiledSessionRepositoryPublication,
    ProfiledSessionRepositoryPublished, ProfiledSessionRunReceipt, ProfiledSessionRunSubmit,
    profiled_session_release_router, profiled_session_router, profiled_session_run_router,
};
pub use repositories::{CreateRepository, RepositoryObject, repositories_router};
pub use resource_manifests::session_resource_manifest_router;
pub use sandbox_policies::environment_extensions_router;
