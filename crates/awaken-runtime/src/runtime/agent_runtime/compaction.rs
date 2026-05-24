use crate::loop_runner::AgentLoopError;

/// Per-run context auto-compaction wiring: shared manager + summarizer that
/// the loop's resolver-wrapper grafts onto every `ResolvedAgent` it produces.
#[derive(Clone)]
#[cfg(feature = "background")]
pub(super) struct CompactionRuntime {
    pub(super) manager: std::sync::Arc<crate::extensions::background::BackgroundTaskManager>,
    summarizer: std::sync::Arc<dyn crate::context::ContextSummarizer>,
}

#[derive(Clone)]
#[cfg(not(feature = "background"))]
pub(super) struct CompactionRuntime;

/// Build the per-run compaction wiring when the preflight agent declared
/// `autocompact_threshold` and no upstream code (builder, custom resolver)
/// already attached a manager + summarizer.
///
/// The manager has its store and owner inbox bound here so background
/// compaction tasks can commit metadata and deliver completion events.
/// `BackgroundTaskPlugin`'s state keys are registered on the store; if a
/// matching plugin is already installed the dup error is treated as a
/// no-op since the keys are already live.
pub(super) fn build_compaction_runtime(
    preflight_resolved: &crate::registry::ResolvedAgent,
    store: &crate::state::StateStore,
    owner_inbox: &crate::inbox::InboxSender,
) -> Result<Option<CompactionRuntime>, AgentLoopError> {
    #[cfg(not(feature = "background"))]
    {
        let _ = (preflight_resolved, store, owner_inbox);
        return Ok(None);
    }

    #[cfg(feature = "background")]
    {
        let opts_in = preflight_resolved
            .context_policy()
            .and_then(|policy| policy.autocompact_threshold)
            .is_some();
        if !opts_in {
            return Ok(None);
        }
        if preflight_resolved.background_manager.is_some()
            && preflight_resolved.context_summarizer.is_some()
        {
            return Ok(None);
        }

        let manager =
            std::sync::Arc::new(crate::extensions::background::BackgroundTaskManager::new());
        manager.set_store(store.clone());
        manager.set_owner_inbox(owner_inbox.clone());

        match store.install_plugin(crate::extensions::background::BackgroundTaskPlugin::new(
            manager.clone(),
        )) {
            Ok(()) => {}
            Err(awaken_contract::StateError::PluginAlreadyInstalled { .. }) => {
                // Keys already registered by an upstream wiring; reuse store as-is.
            }
            Err(awaken_contract::StateError::KeyAlreadyRegistered { .. }) => {
                // A different plugin owns one of the background-task keys; reuse them.
            }
            Err(error) => return Err(AgentLoopError::PhaseError(error)),
        }

        let compaction_config = preflight_resolved
            .spec
            .config::<crate::context::CompactionConfigKey>()
            .unwrap_or_default();
        let summarizer: std::sync::Arc<dyn crate::context::ContextSummarizer> = std::sync::Arc::new(
            crate::context::DefaultSummarizer::with_config(compaction_config),
        );

        Ok(Some(CompactionRuntime {
            manager,
            summarizer,
        }))
    }
}

/// Resolver wrapper that grafts a per-run `BackgroundTaskManager` and
/// `ContextSummarizer` onto every `ResolvedAgent` whose context policy opts
/// in via `autocompact_threshold`. The same `Arc`s are reused across resolve
/// calls so the manager bound during `bind_local_execution_env` is the one
/// used by every subsequent loop step.
pub(super) struct CompactionResolver<'a> {
    inner: &'a dyn crate::registry::ExecutionResolver,
    #[cfg(feature = "background")]
    runtime: CompactionRuntime,
}

impl<'a> CompactionResolver<'a> {
    pub(super) fn new(
        inner: &'a dyn crate::registry::ExecutionResolver,
        runtime: CompactionRuntime,
    ) -> Self {
        #[cfg(feature = "background")]
        {
            Self { inner, runtime }
        }
        #[cfg(not(feature = "background"))]
        {
            let _ = runtime;
            Self { inner }
        }
    }

    #[cfg(feature = "background")]
    fn graft(
        &self,
        mut resolved: crate::registry::ResolvedAgent,
    ) -> crate::registry::ResolvedAgent {
        let opts_in = resolved
            .context_policy()
            .and_then(|policy| policy.autocompact_threshold)
            .is_some();
        if !opts_in {
            return resolved;
        }
        if resolved.background_manager.is_none() {
            resolved.background_manager = Some(self.runtime.manager.clone());
        }
        if resolved.context_summarizer.is_none() {
            resolved.context_summarizer = Some(self.runtime.summarizer.clone());
        }
        resolved
    }
}

impl crate::registry::AgentResolver for CompactionResolver<'_> {
    fn resolve(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedAgent, crate::RuntimeError> {
        #[cfg(not(feature = "background"))]
        {
            return self.inner.resolve(agent_id);
        }
        #[cfg(feature = "background")]
        self.inner
            .resolve(agent_id)
            .map(|resolved| self.graft(resolved))
    }

    fn agent_ids(&self) -> Vec<String> {
        self.inner.agent_ids()
    }
}

impl crate::registry::ExecutionResolver for CompactionResolver<'_> {
    fn resolve_execution(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedExecution, crate::RuntimeError> {
        let execution = self.inner.resolve_execution(agent_id)?;
        #[cfg(not(feature = "background"))]
        {
            return Ok(execution);
        }
        #[cfg(feature = "background")]
        Ok(match execution {
            crate::registry::ResolvedExecution::Local(resolved) => {
                crate::registry::ResolvedExecution::Local(Box::new(self.graft(*resolved)))
            }
            other => other,
        })
    }
}
