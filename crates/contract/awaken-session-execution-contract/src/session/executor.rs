//! Immutable required Runtime roles selected at Session application construction.
//!
//! Each accessor exposes one caller role. All handles come from the same selected
//! Runtime; the binding owns no execution state or durable truth.

use crate::{
    BudgetRuntime, CapabilityRuntime, CompleteSessionRuntime, CoordinationRuntime,
    EnvironmentRuntime, LiveInboxRuntime, OutcomeRuntime, RealizationRuntime,
    SessionCoordinationReads, SessionProjectionSource, SessionRuntime, StreamingRunRuntime,
    TerminalCleanupRuntime, ThreadReadRuntime,
};
use std::sync::Arc;

/// Required Session executor roles. The private owner prevents a caller from
/// silently combining readers and commands from unrelated Runtime owners.
///
/// A core-only Runtime cannot satisfy application composition:
///
/// ```compile_fail,E0277
/// use std::sync::Arc;
/// use awaken_session_execution_contract::{SessionExecutor, SessionRuntime};
///
/// fn cannot_bind_partial<T: SessionRuntime + 'static>(runtime: Arc<T>) {
///     let _: SessionExecutor = runtime.into();
/// }
/// ```
///
/// A selected role cannot be replaced with an unrelated owner:
///
/// ```compile_fail,E0594
/// use std::sync::Arc;
/// use awaken_session_execution_contract::{EnvironmentRuntime, SessionExecutor};
/// fn cannot_replace(executor: &mut SessionExecutor, other: Arc<dyn EnvironmentRuntime>) {
///     executor.environment = other;
/// }
/// ```
#[derive(Clone)]
pub struct SessionExecutor {
    roles: SessionExecutionRoles,
}

/// Required capabilities selected from one executor. The containing
/// `SessionExecutor` exposes this view immutably; roles cannot be replaced.
#[derive(Clone)]
pub struct SessionExecutionRoles {
    pub projection: Arc<dyn SessionProjectionSource>,
    pub core: Arc<dyn SessionRuntime>,
    pub realization: Arc<dyn RealizationRuntime>,
    pub coordination: Arc<dyn CoordinationRuntime>,
    pub coordination_reads: Arc<dyn SessionCoordinationReads>,
    pub budget: Arc<dyn BudgetRuntime>,
    pub streaming_runs: Arc<dyn StreamingRunRuntime>,
    pub environment: Arc<dyn EnvironmentRuntime>,
    pub thread_reads: Arc<dyn ThreadReadRuntime>,
    pub terminal_cleanup: Arc<dyn TerminalCleanupRuntime>,
    pub outcomes: Arc<dyn OutcomeRuntime>,
    pub live_inbox: Arc<dyn LiveInboxRuntime>,
    pub capability: Arc<dyn CapabilityRuntime>,
}

impl std::ops::Deref for SessionExecutor {
    type Target = SessionExecutionRoles;
    fn deref(&self) -> &Self::Target {
        &self.roles
    }
}

impl SessionExecutor {
    /// Committed projection facts without execution authority.
    pub fn projection(&self) -> &dyn SessionProjectionSource {
        self.projection.as_ref()
    }

    /// The required `SessionRuntime` selected at construction.
    pub fn core(&self) -> &dyn SessionRuntime {
        self.core.as_ref()
    }

    /// The required `RealizationRuntime` selected at construction.
    pub fn realization(&self) -> &dyn RealizationRuntime {
        self.realization.as_ref()
    }

    /// The required `CoordinationRuntime` selected at construction.
    pub fn coordination(&self) -> &dyn CoordinationRuntime {
        self.coordination.as_ref()
    }

    /// The required `SessionCoordinationReads` selected at construction.
    pub fn coordination_reads(&self) -> &dyn SessionCoordinationReads {
        self.coordination_reads.as_ref()
    }

    /// The required `BudgetRuntime` selected at construction.
    pub fn budget(&self) -> &dyn BudgetRuntime {
        self.budget.as_ref()
    }

    /// The required `StreamingRunRuntime` selected at construction.
    pub fn streaming_runs(&self) -> &dyn StreamingRunRuntime {
        self.streaming_runs.as_ref()
    }

    /// The required `EnvironmentRuntime` selected at construction.
    pub fn environment(&self) -> &dyn EnvironmentRuntime {
        self.environment.as_ref()
    }

    /// The required `ThreadReadRuntime` selected at construction.
    pub fn thread_reads(&self) -> &dyn ThreadReadRuntime {
        self.thread_reads.as_ref()
    }

    /// The required `TerminalCleanupRuntime` selected at construction.
    pub fn terminal_cleanup(&self) -> &dyn TerminalCleanupRuntime {
        self.terminal_cleanup.as_ref()
    }

    /// The required `OutcomeRuntime` selected at construction.
    pub fn outcomes(&self) -> &dyn OutcomeRuntime {
        self.outcomes.as_ref()
    }

    /// The required `LiveInboxRuntime` selected at construction.
    pub fn live_inbox(&self) -> &dyn LiveInboxRuntime {
        self.live_inbox.as_ref()
    }

    /// The required `CapabilityRuntime` selected at construction.
    pub fn capability(&self) -> &dyn CapabilityRuntime {
        self.capability.as_ref()
    }
}

impl<T> From<Arc<T>> for SessionExecutor
where
    T: CompleteSessionRuntime + 'static,
{
    fn from(runtime: Arc<T>) -> Self {
        let runtime: Arc<dyn CompleteSessionRuntime> = runtime;
        Self::from(runtime)
    }
}

impl From<Arc<dyn CompleteSessionRuntime>> for SessionExecutor {
    fn from(runtime: Arc<dyn CompleteSessionRuntime>) -> Self {
        Self {
            roles: SessionExecutionRoles {
                projection: runtime.clone(),
                core: runtime.clone(),
                realization: runtime.clone(),
                coordination: runtime.clone(),
                coordination_reads: runtime.clone(),
                budget: runtime.clone(),
                streaming_runs: runtime.clone(),
                environment: runtime.clone(),
                thread_reads: runtime.clone(),
                terminal_cleanup: runtime.clone(),
                outcomes: runtime.clone(),
                live_inbox: runtime.clone(),
                capability: runtime.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_roles_and_clones_keep_one_runtime_owner() {
        // Every command/read role must refer to the selected owner, including
        // after cloning the executor. Mixing owners could join unrelated
        // projection cursors and execution fences despite valid individual ports.
        let runtime = Arc::new(crate::NeutralRuntime);
        let owner = Arc::as_ptr(&runtime).cast::<()>();
        let selected = SessionExecutor::from(runtime);
        for executor in [&selected, &selected.clone()] {
            for role in [
                Arc::as_ptr(&executor.core).cast::<()>(),
                Arc::as_ptr(&executor.projection).cast::<()>(),
                Arc::as_ptr(&executor.realization).cast::<()>(),
                Arc::as_ptr(&executor.coordination).cast::<()>(),
                Arc::as_ptr(&executor.coordination_reads).cast::<()>(),
                Arc::as_ptr(&executor.budget).cast::<()>(),
                Arc::as_ptr(&executor.streaming_runs).cast::<()>(),
                Arc::as_ptr(&executor.environment).cast::<()>(),
                Arc::as_ptr(&executor.thread_reads).cast::<()>(),
                Arc::as_ptr(&executor.terminal_cleanup).cast::<()>(),
                Arc::as_ptr(&executor.outcomes).cast::<()>(),
                Arc::as_ptr(&executor.live_inbox).cast::<()>(),
                Arc::as_ptr(&executor.capability).cast::<()>(),
            ] {
                assert_eq!(role, owner);
            }
        }
    }
}
