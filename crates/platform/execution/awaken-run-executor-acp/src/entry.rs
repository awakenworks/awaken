// @formal surface requirement=protocol.acp.acp_normalization
// @formal reason: Uses the canonical ThreadCommit before ACP channel entry and keeps physical cleanup in its existing owner.
//! ACP attempt entry over the existing committed Run authority.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

#[derive(Debug, Clone)]
pub(super) struct PermissionResume {
    pub(super) call_id: String,
    pub(super) tool_id: String,
    pub(super) arguments: serde_json::Value,
    pub(super) allow: bool,
}

impl AcpRunExecutor {
    pub(super) async fn execute_with_permission_resume(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
        permission_resume: Option<PermissionResume>,
        accepted_resume: Option<ResumeCommit>,
    ) -> Result<RunState> {
        let mut context = activation.narrow_context(context);
        if let Some(state @ RunState::Ended(_)) = context
            .reader
            .as_ref()
            .and_then(|reader| reader.run_state(&activation.run_id))
        {
            return Ok(state);
        }
        let prepared = self.prepare_owned_tools(&activation, &mut context).await?;
        if let Some(state) = prepared.terminal {
            return Ok(state);
        }
        // The actual source owns enforcement, not a snapshot's backend label.
        // Unknown/rejected profiles still stop before home restore or launch.
        if !prepared.original_capability_known
            || self
                .source
                .check_tool_narrowing(&context.tool_capability_narrowing)
                .is_err()
        {
            let disposition = RunDisposition::ended(
                activation.run_id.clone(),
                EndCause::Error(Failure::CapabilityBound),
            );
            let state = disposition.state();
            commit(
                &context,
                Vec::new(),
                permission_resume.as_ref(),
                &activation.thread_id,
                disposition,
                activation.input,
                Vec::new(),
            )
            .await?;
            return Ok(state);
        }
        commit::begin_running(&context, &activation, accepted_resume.as_ref()).await?;
        // Restore the CLI's portable session-home before launch and harvest it after,
        // so a local-dir CLI's session recovers across directories/machines. A
        // Gateway (server-side) or stateless adapter has no local session-home → the
        // resolution returns `None` and this is a no-op (as is the default provider).
        let home = self.session_home_binding(&activation);
        if let Some((key, plan)) = &home {
            verify_attempt_ownership(context.ownership.as_deref()).await?;
            self.session_home.restore(key, plan).await;
        }
        let mut cleanup = CleanupScope::new(context.physical_quiescence.clone());
        let result = self
            .drive(activation, &context, permission_resume, &mut cleanup)
            .await;
        // A committed business outcome may precede physical exit. Keep the
        // same structured execution and channel alive until cleanup observes
        // that exit; the Dispatch owner cannot ACK merely because drive ended.
        cleanup.finish(self.policy.reap_grace).await;
        if let Some((key, plan)) = &home {
            match verify_attempt_ownership(context.ownership.as_deref()).await {
                Ok(()) => self.session_home.harvest(key, plan).await,
                Err(error) if result.is_ok() => return Err(error),
                Err(_) => {}
            }
        }
        result
    }
}
