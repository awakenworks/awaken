//! Application-facing Session ports that do not own durable state.
//!
//! Session Core content is named from its owner, `awaken-session-contract`;
//! this contract publishes no Core alias:
//!
//! ```compile_fail
//! use awaken_session_application_contract::{SessionEventAdmission};
//! ```
//!
//! ```compile_fail
//! use awaken_session_application_contract::{LifecycleFactDelivery};
//! ```
//!
//! ```compile_fail
//! use awaken_session_application_contract::{session_usage_from_thread_usage};
//! ```
//!
//! The contract holds ports only. Wall-clock codecs are named from
//! `awaken_time::utc`, and the Session application's process configuration
//! from `awaken-session-application`:
//!
//! ```compile_fail
//! use awaken_session_application_contract::{epoch_millis_to_rfc3339};
//! ```
//!
//! ```compile_fail
//! use awaken_session_application_contract::{SessionApplicationConfiguration};
//! ```
//!
//! # Boundary
//!
//! Between callers and the Session use cases (the narrow port protocol adapters drive).
//!
//! Implemented by: `awaken-coordinator`, `awaken-environment-application`,
//! `awaken-process-startup`, `awaken-protocol-managed-testkit`, `awaken-session-application`,
//! `awaken-session-execution-contract`, `awaken-session-executor`, `awaken-session-run-delivery`.
//!
//! Called by: `awaken-cli`, `awaken-coordinator`, `awaken-coordinator-client`,
//! `awaken-deployment-application`, `awaken-dispatch-contract`, `awaken-dispatch-erasure`,
//! `awaken-dispatch-http`, `awaken-environment-application`, `awaken-managed-session-events`,
//! `awaken-process-startup`, `awaken-protocol-a2a`, `awaken-protocol-ag-ui`,
//! `awaken-protocol-ai-sdk`, `awaken-protocol-awaken`, `awaken-protocol-managed`,
//! `awaken-protocol-managed-testkit`, `awaken-session-application`,
//! `awaken-session-execution-contract`, `awaken-session-executor`, `awaken-session-realization`,
//! `awaken-session-run-delivery`.

mod execution_observation;
pub use execution_observation::{
    SessionRunDispatchObservation, SessionRunDispatchQueries, SessionRunExecutionPhase,
};
mod executable_projection;
mod lifecycle_notifier;
mod live_inbox;
mod run_application;
mod run_coordination;
mod task_attention;

pub(crate) use awaken_session_types::SessionId;

pub use executable_projection::{
    ExecutableAgentProjectionRefresh, ExecutableEnvironmentProjectionRefresh,
    ExecutableProjectionAdmission, ExecutableProjectionRefresh,
    process_local_executable_projection_admission,
};

pub use lifecycle_notifier::LifecycleFactNotifier;
pub use live_inbox::{
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot,
};
pub use run_application::{SessionRunBackgroundApplication, SessionRunReplacementApplication};
pub use run_coordination::{
    DelegatedRunSnapshot, SessionBudgetResumeDelivery, SessionBudgetResumeDisposition,
    SessionBudgetResumeTicket,
};
pub use task_attention::{
    PRIVATE_TASK_INPUT_VERSION, PrivateTaskInput, PrivateTaskInputError, RegisteredTaskControl,
    TASK_TENURE_RECORD_VERSION, TaskAttention, TaskAttentionAck, TaskReadPartition, TaskTenureRead,
    TaskTenureRecord, TaskTenureRecordError, WORKER_TASK_ATTENTION_PATH, WORKER_TASK_TENURE_PATH,
    registered_task_holder,
};

mod worker_lease_value;
pub use worker_lease_value::{SessionWorkGrant, WorkLeaseEpoch};

mod worker_lease;
pub use worker_lease::{
    SessionWorkAuthorityReceipt, SessionWorkerLeaseAuthority, acquire_session_work_receipt,
};

mod worker_lease_types;
use worker_lease_types::SessionWork;
pub use worker_lease_types::{
    SessionWorkAcquisition, SessionWorkOwnership, session_work_revival_is_authorized,
};
mod worker_assignment;
pub use worker_assignment::{SessionWorkerAssignment, SessionWorkerLeases};
