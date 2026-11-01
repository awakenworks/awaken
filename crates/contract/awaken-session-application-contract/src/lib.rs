//! Application-facing Session ports that do not own durable state.
//!
//! Session Core content is named from its owner, `awaken-session-contract`;
//! this contract publishes no Core alias:
//!
//! ```compile_fail
//! use awaken_session_application_contract::SessionEventAdmission;
//! ```
//!
//! ```compile_fail
//! use awaken_session_application_contract::LifecycleFactDelivery;
//! ```
//!
//! ```compile_fail
//! use awaken_session_application_contract::session_usage_from_thread_usage;
//! ```
//!
//! The contract holds ports only. Wall-clock codecs are named from
//! `awaken_time::utc`, and the Session application's process configuration
//! from `awaken-session-application`:
//!
//! ```compile_fail
//! use awaken_session_application_contract::epoch_millis_to_rfc3339;
//! ```
//!
//! ```compile_fail
//! use awaken_session_application_contract::SessionApplicationConfiguration;
//! ```

mod execution_observation;
pub use execution_observation::{
    SessionRunDispatchObservation, SessionRunDispatchQueries, SessionRunExecutionPhase,
};
mod executable_projection;
mod lifecycle_notifier;
mod live_inbox;
mod pending;
mod run_application;
mod run_coordination;
mod step_outcome;
mod task_attention;

pub(crate) use awaken_session_types::RunError;
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
pub use pending::Pending;
pub use run_application::{
    EventForwardingSink, OwnerScopedRunApplication, RunApplication, RunApplicationError, RunMode,
    RunResume, SessionRunBackgroundApplication, SessionRunReplacementApplication, StopReceipt,
    blocks_text, paginate_history,
};
pub use run_coordination::{
    DelegatedRun, DelegatedRunSnapshot, SessionBudgetResumeDelivery,
    SessionBudgetResumeDisposition, SessionBudgetResumeTicket,
};
pub use step_outcome::StepOutcome;
pub use task_attention::{
    PRIVATE_TASK_INPUT_VERSION, PrivateTaskInput, PrivateTaskInputError, TaskAttention,
    TaskAttentionAck, WORKER_TASK_ATTENTION_PATH, registered_task_holder,
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
