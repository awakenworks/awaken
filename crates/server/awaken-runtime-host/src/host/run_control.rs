//! Process selection and committed-reader access for the canonical dispatch control owner.
use super::{HostError, SessionCtx, SharedHost};
use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
use awaken_dispatch_execution::durable_control::{
    DurableRunControl, decision_input, manual_resume_input, pause_resident, wake_resident,
};
use awaken_runtime::Runtime;
impl SharedHost {
    fn resident_context(&self, thread: &str) -> Option<std::sync::Arc<SessionCtx>> {
        self.installed_sessions
            .read_run(thread, |run| run.context.clone())
            .flatten()
    }
    fn durable_run_control(&self) -> Result<DurableRunControl, HostError> {
        Ok(DurableRunControl::new(
            self.coordinator_dispatch()?,
            self.dispatch.dispatch_pool.get().cloned(),
        ))
    }
    pub(crate) async fn persist_dispatch_cancellation(
        &self,
        run: &RunId,
        live: Option<&Runtime>,
    ) -> Result<bool, HostError> {
        self.durable_run_control()?
            .persist_cancellation(run, live)
            .await
    }
    pub(crate) async fn persist_recoverable_dispatch_cancellation(
        &self,
        run: &RunId,
        live: Option<&Runtime>,
    ) -> Result<bool, HostError> {
        self.durable_run_control()?
            .persist_recoverable_cancellation(run, live)
            .await
    }
    pub async fn cancel_durable(&self, thread: &str, run: &str) -> Result<(), HostError> {
        let resident = self.resident_context(thread);
        self.dispatch_pool_or_err()?;
        self.durable_run_control()?
            .cancel(
                &RunId(run.to_owned()),
                resident.as_ref().map(|ctx| ctx.runtime.as_ref()),
            )
            .await
    }
    pub async fn wake_durable(&self, thread: &str, run: &str) -> Result<(), HostError> {
        self.require_durable_delivery()?;
        let runtime = self
            .resident_context(thread)
            .filter(|ctx| ctx.delivery.is_durable())
            .map(|ctx| ctx.claimed_worker.runtime().clone());
        wake_resident(runtime, run).await
    }
    pub async fn pause_durable(
        &self,
        thread: &str,
        run: Option<&str>,
    ) -> Result<String, HostError> {
        self.require_durable_delivery()?;
        let resident = self.resident_context(thread);
        pause_resident(
            resident.as_ref().map(|ctx| ctx.runtime.clone()),
            resident
                .as_ref()
                .map(|ctx| ctx.claimed_worker.runtime().clone()),
            &ThreadId(thread.to_owned()),
            resident
                .as_ref()
                .is_some_and(|ctx| ctx.delivery.is_durable()),
            run,
        )
        .await
    }
    pub async fn stage_decision(&self, thread: &str, allow: bool) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        self.dispatch_pool_or_err()?;
        let thread = ThreadId(thread.to_owned());
        let awaiting = ctx
            .commit
            .open_wait_for_thread(&thread)
            .await
            .map_err(HostError::internal)?;
        let input = decision_input(thread, awaiting, allow)?;
        self.durable_run_control()?.stage_input(input).await
    }
    pub async fn stage_manual_resume(
        &self,
        thread: &str,
        text: String,
    ) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        self.dispatch_pool_or_err()?;
        let thread = ThreadId(thread.to_owned());
        let awaiting = ctx
            .commit
            .open_wait_for_thread(&thread)
            .await
            .map_err(HostError::internal)?;
        let input = manual_resume_input(thread, awaiting, text)?;
        self.durable_run_control()?.stage_input(input).await
    }
}
