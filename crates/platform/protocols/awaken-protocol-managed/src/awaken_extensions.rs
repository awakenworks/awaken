//! Awaken's extensions of the Managed Agents protocol, namespaced under
//! `/v1/awaken`: route definitions and request/response shapes for profiled
//! Sessions and their Runs, resource manifests, content-capture and execution
//! evidence reads, event-admission closing, Repositories and Memory copy
//! conflicts. They speak the Managed protocol's error envelope, public ids and
//! headers, so they live in this adapter; their handlers are this crate's.

mod content_capture;
mod event_admissions;
mod execution_evidence;
mod memory_copy_conflicts;
mod profiled_run_events;
mod profiled_sessions;
mod repositories;
mod resource_manifests;

pub use content_capture::session_content_capture_router;
pub use event_admissions::{
    EventAdmissionProof, EventAdmissionReceipt, EventAdmissionStatus,
    session_event_admission_router,
};
pub use execution_evidence::{
    SessionExecutionEvidence, SessionRunExecutionEvidence, session_execution_evidence_router,
};
pub use memory_copy_conflicts::memory_copy_conflicts_router;
pub use profiled_run_events::{
    ProfiledSessionRunEventsQuery, ProfiledSessionRunResume, profiled_session_run_events_router,
    profiled_session_run_resume_router,
};
pub use profiled_sessions::{
    ProfiledSessionCreate, ProfiledSessionCreated, ProfiledSessionMcpAttachment,
    ProfiledSessionMode, ProfiledSessionRelease, ProfiledSessionReleased,
    ProfiledSessionRepository, ProfiledSessionRepositoryPublication,
    ProfiledSessionRepositoryPublished, ProfiledSessionRunReceipt, ProfiledSessionRunSubmit,
    profiled_session_release_router, profiled_session_router, profiled_session_run_router,
};
pub use repositories::{CreateRepository, RepositoryObject, repositories_router};
pub use resource_manifests::session_resource_manifest_router;
