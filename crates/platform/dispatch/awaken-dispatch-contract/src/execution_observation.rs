// @formal surface exclude=read_model_contract
// @formal reason: Read-only projection port over original Dispatch evidence; grants no claim, execution or closure authority.
//! Delivery owner's physical read projection into Session's consumer language.
use crate::{DispatchError, DispatchOperationalFeed};
use awaken_agent_contract::{ThreadId, agent::run::Id as RunId};
use awaken_session_application_contract::SessionRunExecutionPhase;
#[async_trait::async_trait]
pub trait DispatchExecutionReads: DispatchOperationalFeed {
    async fn observe_execution(
        &self,
        run: &RunId,
        thread: &ThreadId,
    ) -> Result<SessionRunExecutionPhase, DispatchError>;
}
