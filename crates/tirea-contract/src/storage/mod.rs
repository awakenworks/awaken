//! Persistent Thread storage contracts and abstractions.

pub mod mailbox_traits;
pub mod mailbox_types;
pub mod run_traits;
pub mod run_types;
pub mod task_traits;
pub mod task_types;
pub mod traits;
pub mod types;

pub use crate::runtime::RunStatus;
pub use mailbox_traits::{MailboxReader, MailboxStore, MailboxWriter};
pub use mailbox_types::{
    has_active_claim_for_mailbox, paginate_mailbox_entries, MailboxEntry, MailboxEntryOrigin,
    MailboxEntryStatus, MailboxInterrupt, MailboxPage, MailboxQuery, MailboxReceiver, MailboxState,
    MailboxStoreError, ReceiveOutcome,
};
pub use run_traits::{RunReader, RunStore, RunWriter};
pub use run_types::{
    paginate_runs_in_memory, RunOrigin, RunPage, RunQuery, RunRecord, RunStoreError,
};
pub use task_traits::{TaskReader, TaskStore, TaskWriter};
pub use task_types::{
    paginate_tasks, Task, TaskPage, TaskQuery, TaskStatus, TaskStoreError, TaskUpdate,
};
pub use traits::{ThreadReader, ThreadStore, ThreadSync, ThreadWriter};
pub use types::{
    paginate_in_memory, Committed, MessagePage, MessageQuery, MessageWithCursor, SortOrder,
    ThreadHead, ThreadListPage, ThreadListQuery, ThreadStoreError, VersionPrecondition,
};
