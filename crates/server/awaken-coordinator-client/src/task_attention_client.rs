//! A registered Worker incarnation's Task attention delivery.
//!
//! The holder this incarnation tenures its BackgroundTasks to and the identity
//! that authenticates their attention come from the one registered
//! [`WorkerIdentity`], so the Coordinator's holder check (rows A2/A3) admits
//! exactly the Tasks this incarnation holds.

use awaken_dispatch_contract::WorkerIdentity;
use awaken_session_application_contract::{
    TaskAttention, TaskAttentionAck, TaskAttentionDelivery, registered_task_holder,
};
use awaken_session_types::RunError;

use crate::WorkerControlClient;

/// Delivers this incarnation's Task observations over the authenticated
/// Worker control transport.
pub struct WorkerTaskAttentionClient {
    control: WorkerControlClient,
    identity: WorkerIdentity,
    holder: String,
}

impl WorkerTaskAttentionClient {
    /// The delivery of the registered incarnation `identity`.
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
impl TaskAttentionDelivery for WorkerTaskAttentionClient {
    fn holder(&self) -> &str {
        &self.holder
    }

    async fn deliver(&self, attention: TaskAttention) -> Result<TaskAttentionAck, RunError> {
        self.control
            .deliver_task_attention(&self.identity, &attention)
            .await
    }
}
