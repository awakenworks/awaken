// @formal entrypoint executable_projection.failed_refresh_never_enters_handler
// @formal entrypoint executable_projection.runtime_admission_requires_both_refreshes
//! Ordered admission over the two rebuildable executable projections.

use std::sync::Arc;

/// Agent-domain durable projection adapter. The adapter retains its cursor,
/// mutation lock, command codec, and canonical catalog state machine.
#[async_trait::async_trait]
pub trait ExecutableAgentProjectionRefresh: Send + Sync {
    async fn refresh_agent_projection(&self) -> Result<(), String>;
}

/// Environment-domain durable projection adapter. The adapter retains its
/// cursor, mutation lock, command codec, and canonical catalog state machine.
#[async_trait::async_trait]
pub trait ExecutableEnvironmentProjectionRefresh: Send + Sync {
    async fn refresh_environment_projection(&self) -> Result<(), String>;
}

/// The only process-composed prerequisite for consumers of executable facts.
/// It owns no cursor, cache, or durable state. Distinct typed domain ports make
/// a partial or duplicated adapter pair unrepresentable after construction.
pub struct ExecutableProjectionRefresh {
    agents: Arc<dyn ExecutableAgentProjectionRefresh>,
    environments: Arc<dyn ExecutableEnvironmentProjectionRefresh>,
}

impl ExecutableProjectionRefresh {
    #[must_use]
    pub fn new(
        agents: Arc<dyn ExecutableAgentProjectionRefresh>,
        environments: Arc<dyn ExecutableEnvironmentProjectionRefresh>,
    ) -> Self {
        Self {
            agents,
            environments,
        }
    }

    /// Refresh Agent then Environment and mint the only cross-domain admission
    /// capability. Neither domain error can produce a capability.
    pub async fn refresh(&self) -> Result<ExecutableProjectionAdmission, String> {
        let agents = agent_refresh_completed(self.agents.refresh_agent_projection().await)?;
        environment_refresh_completed(
            agents,
            self.environments.refresh_environment_projection().await,
        )
    }
}

/// Proof that both ordered durable refresh calls completed. Its constructors
/// remain inside this module, so downstream code can enter a handler only from
/// the canonical pair or the explicit process-local AllInOne topology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct ExecutableProjectionAdmission {
    source: ExecutableProjectionAdmissionSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecutableProjectionAdmissionSource {
    DurablePair,
    ProcessLocal,
}

impl ExecutableProjectionAdmission {
    /// Enter a synchronous handler boundary while carrying the capability.
    /// Returning a Future from the closure keeps the same gate usable for async
    /// handlers without adding another middleware or admission state machine.
    pub fn enter<T>(&self, handler: impl FnOnce() -> T) -> T {
        handler()
    }
}

struct AgentProjectionRefreshed;

fn agent_refresh_completed<E>(result: Result<(), E>) -> Result<AgentProjectionRefreshed, E> {
    result.map(|()| AgentProjectionRefreshed)
}

fn environment_refresh_completed<E>(
    _agents: AgentProjectionRefreshed,
    result: Result<(), E>,
) -> Result<ExecutableProjectionAdmission, E> {
    result.map(|()| ExecutableProjectionAdmission {
        source: ExecutableProjectionAdmissionSource::DurablePair,
    })
}

/// AllInOne registration and consumption share the exact process-local catalog
/// instances, so no durable projection adapters exist in that topology.
pub fn process_local_executable_projection_admission() -> ExecutableProjectionAdmission {
    ExecutableProjectionAdmission {
        source: ExecutableProjectionAdmissionSource::ProcessLocal,
    }
}

#[cfg(any(test, kani))]
fn check_paired_refresh_admission(agent: Result<(), ()>, environment: Result<(), ()>) {
    let should_enter = agent.is_ok() && environment.is_ok();
    let admission = agent_refresh_completed(agent)
        .and_then(|agents| environment_refresh_completed(agents, environment));
    let entered = std::cell::Cell::new(false);
    if let Ok(admission) = admission {
        admission.enter(|| entered.set(true));
    }
    assert_eq!(entered.get(), should_enter);
}

#[cfg(kani)]
/// @obligation executable_projection.failed_refresh_never_enters_handler
/// @obligation executable_projection.runtime_admission_requires_both_refreshes
#[kani::proof]
fn executable_projection_admission_requires_both_ordered_refreshes() {
    // No assumptions or stubs: both actual Result shapes cross the production
    // capability constructors and the production handler-entry method.
    check_paired_refresh_admission(kani::any(), kani::any());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_refresh_capability_covers_every_domain_outcome() {
        // Causes: C1 Agent refresh succeeds/fails; C2 Environment refresh
        // succeeds/fails. Effects: E1 mint the one durable-pair admission and
        // enter the handler; E2 return failure with no handler entry.
        // Constraint: Environment completion consumes the private Agent
        // completion, so it cannot mint admission independently. Decision
        // table: D1 success+success=>E1; D2 failure+success=>E2; D3
        // success+failure=>E2; D4 failure+failure=>E2.
        check_paired_refresh_admission(Ok(()), Ok(()));
        check_paired_refresh_admission(Err(()), Ok(()));
        check_paired_refresh_admission(Ok(()), Err(()));
        check_paired_refresh_admission(Err(()), Err(()));
    }
}
