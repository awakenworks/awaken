//! Application-facing Session ports that do not own durable state.

mod event_admission;
mod executable_projection;
mod lifecycle_delivery;
mod pending;
mod run_application;
mod run_coordination;
mod step_outcome;
mod usage;

pub(crate) use awaken_session_event_contract::DeletedSessionEventAdmissions;
#[cfg(any(test, kani))]
pub(crate) use awaken_session_event_contract::RetainedEventAdmission;
pub(crate) use awaken_session_event_contract::{
    SessionEventBatch, SessionEventBatchAdmissionClass, classify_session_event_batch_admission,
};
pub(crate) use awaken_session_types::{RunError, RunErrorKind};
pub(crate) use awaken_session_types::{SessionId, SessionRevision};

pub use event_admission::{
    SessionEventAdmission, event_admission_closed, event_admission_conflict,
    resolve_deleted_event_admission,
};
pub use executable_projection::{
    ExecutableAgentProjectionRefresh, ExecutableEnvironmentProjectionRefresh,
    ExecutableProjectionAdmission, ExecutableProjectionRefresh,
    process_local_executable_projection_admission,
};

pub use lifecycle_delivery::{
    CompositeLifecycleFactDelivery, LifecycleFactDelivery, LifecycleFactNotifier,
};
pub use pending::Pending;
pub use run_application::{
    CursorParams, EventForwardingSink, HistoryPage, OwnerScopedRunApplication, RunApplication,
    RunApplicationError, RunMode, RunResume, SessionRunBackgroundApplication,
    SessionRunReplacementApplication, blocks_text, epoch_millis_to_rfc3339, paginate_history,
    project_epoch_millis, rfc3339_to_epoch_millis,
};
pub use run_coordination::{
    DelegatedRun, DelegatedRunSnapshot, SessionBudgetResumeDelivery,
    SessionBudgetResumeDisposition, SessionBudgetResumeTicket,
};
pub use step_outcome::StepOutcome;
pub use usage::session_usage_from_thread_usage;

mod composition;
mod session_services;
pub use composition::{
    SessionApplicationBuildError, SessionApplicationConfiguration, SessionExecutionPlacement,
    WorkDispatchFailure, WorkDispatchReconciliation, runtime_placement_for_topology,
};
pub use session_services::{
    CredentialMaterialIngress, CredentialMaterialIngressCommand, CredentialMaterialIngressReceipt,
    CredentialMaterialInput, CredentialMaterialRetirementCommand,
    CredentialMaterialRotationCommand, CredentialPlaintext, SessionCredentialAccessRequest,
    SessionCredentialSource, SessionParticipantProvenance,
};
