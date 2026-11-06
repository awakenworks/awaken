//! A registered Worker incarnation's Task control.
//!
//! The holder this incarnation tenures its BackgroundTasks to and the identity
//! that authenticates their attention and tenure reads come from the one
//! registered [`WorkerIdentity`], so the Coordinator's holder check (rows
//! A2/A3, H4) admits exactly the Tasks this incarnation holds.

use awaken_runtime_contract::runs::RunError;
use awaken_session_application_contract::{
    RegisteredTaskControl, TaskAttention, TaskAttentionAck, TaskTenureRead, TaskTenureRecord,
    registered_task_holder,
};
use awaken_worker_contract::WorkerIdentity;

use crate::WorkerControlClient;

/// Delivers this incarnation's Task observations and reads its held Tasks'
/// tenure over the authenticated Worker control transport.
pub struct WorkerTaskControlClient {
    control: WorkerControlClient,
    identity: WorkerIdentity,
    holder: String,
}

impl WorkerTaskControlClient {
    /// The Task control of the registered incarnation `identity`.
    #[must_use]
    pub fn new(control: WorkerControlClient, identity: WorkerIdentity) -> Self {
        let holder = registered_task_holder(&identity.worker_id, &identity.incarnation_id);
        Self {
            control,
            identity,
            holder,
        }
    }
}

#[async_trait::async_trait]
impl RegisteredTaskControl for WorkerTaskControlClient {
    fn holder(&self) -> &str {
        &self.holder
    }

    async fn deliver(&self, attention: TaskAttention) -> Result<TaskAttentionAck, RunError> {
        self.control
            .deliver_task_attention(&self.identity, &attention)
            .await
    }

    async fn read_tenure(
        &self,
        read: TaskTenureRead,
    ) -> Result<Option<TaskTenureRecord>, RunError> {
        self.control.read_task_tenure(&self.identity, &read).await
    }
}
