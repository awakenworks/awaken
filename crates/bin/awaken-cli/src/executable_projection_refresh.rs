//! One active-active refresh boundary for Coordinator executable projections.

use std::sync::Arc;

use awaken_executable_agent_catalog::PostgresExecutableAgentRegistrar;
use awaken_executable_environment_catalog::PostgresExecutableEnvironmentRegistrar;
use awaken_session_contract::ExecutableProjectionRefresh;

struct AgentProjectionRefresh(Arc<PostgresExecutableAgentRegistrar>);

#[async_trait::async_trait]
impl ExecutableProjectionRefresh for AgentProjectionRefresh {
    async fn refresh(&self) -> Result<(), String> {
        self.0
            .refresh_projection()
            .await
            .map_err(|error| error.to_string())
    }
}

struct EnvironmentProjectionRefresh(Arc<PostgresExecutableEnvironmentRegistrar>);

#[async_trait::async_trait]
impl ExecutableProjectionRefresh for EnvironmentProjectionRefresh {
    async fn refresh(&self) -> Result<(), String> {
        self.0
            .refresh_projection()
            .await
            .map_err(|error| error.to_string())
    }
}

/// One ordered process adapter over the existing Agent and Environment
/// high-water reconcilers. It owns no cursor or catalog state.
struct ExecutableProjectionRefreshers {
    agents: Option<Arc<dyn ExecutableProjectionRefresh>>,
    environments: Option<Arc<dyn ExecutableProjectionRefresh>>,
}

impl ExecutableProjectionRefreshers {
    fn from_refreshers(
        agents: Option<Arc<dyn ExecutableProjectionRefresh>>,
        environments: Option<Arc<dyn ExecutableProjectionRefresh>>,
    ) -> Option<Arc<Self>> {
        (agents.is_some() || environments.is_some()).then(|| {
            Arc::new(Self {
                agents,
                environments,
            })
        })
    }
}

#[async_trait::async_trait]
impl ExecutableProjectionRefresh for ExecutableProjectionRefreshers {
    async fn refresh(&self) -> Result<(), String> {
        if let Some(agents) = &self.agents {
            agents.refresh().await?;
        }
        if let Some(environments) = &self.environments {
            environments.refresh().await?;
        }
        Ok(())
    }
}

/// Build the one process-wide active-active reconciliation handle. AllInOne
/// shares process-local catalogs and therefore returns no handle.
pub(crate) fn shared(
    agents: Option<Arc<PostgresExecutableAgentRegistrar>>,
    environments: Option<Arc<PostgresExecutableEnvironmentRegistrar>>,
) -> Option<Arc<dyn ExecutableProjectionRefresh>> {
    ExecutableProjectionRefreshers::from_refreshers(
        agents.map(|value| Arc::new(AgentProjectionRefresh(value)) as Arc<_>),
        environments.map(|value| Arc::new(EnvironmentProjectionRefresh(value)) as Arc<_>),
    )
    .map(|refresh| refresh as Arc<dyn ExecutableProjectionRefresh>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingRefresh {
        calls: Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
        result: Result<(), String>,
    }

    #[async_trait::async_trait]
    impl ExecutableProjectionRefresh for RecordingRefresh {
        async fn refresh(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push(self.name);
            self.result.clone()
        }
    }

    fn recorder(
        calls: &Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
        result: Result<(), &str>,
    ) -> Arc<dyn ExecutableProjectionRefresh> {
        Arc::new(RecordingRefresh {
            calls: calls.clone(),
            name,
            result: result.map_err(str::to_string),
        })
    }

    #[tokio::test]
    async fn composite_refresh_is_ordered_fail_closed_and_absent_for_all_in_one() {
        // Causes: C1 Agent refresh succeeds/fails; C2 Environment refresh
        // succeeds/fails; C3 neither durable adapter is installed. Effects: E1
        // invoke Agent then Environment and succeed; E2 stop at the first
        // failure; E3 construct no composite. Constraints: K1 the composite
        // owns no cursor or cache; K2 Environment never runs after Agent fails.
        // Decision table: D1 TT=>[A,E]/Ok; D2 F-=>[A]/Err;
        // D3 TF=>[A,E]/Err; D4 absent=>None.
        async fn exercise(
            agent: Result<(), &str>,
            environment: Result<(), &str>,
        ) -> (Vec<&'static str>, Result<(), String>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let refresh = ExecutableProjectionRefreshers::from_refreshers(
                Some(recorder(&calls, "agent", agent)),
                Some(recorder(&calls, "environment", environment)),
            )
            .unwrap();
            let result = refresh.refresh().await;
            let recorded = calls.lock().unwrap().clone();
            (recorded, result)
        }

        assert_eq!(
            exercise(Ok(()), Ok(())).await,
            (vec!["agent", "environment"], Ok(())),
            "D1"
        );
        let (calls, result) = exercise(Err("agent offline"), Ok(())).await;
        assert_eq!(calls, vec!["agent"], "D2");
        assert_eq!(result, Err("agent offline".into()), "D2");
        let (calls, result) = exercise(Ok(()), Err("environment offline")).await;
        assert_eq!(calls, vec!["agent", "environment"], "D3");
        assert_eq!(result, Err("environment offline".into()), "D3");
        assert!(
            ExecutableProjectionRefreshers::from_refreshers(None, None).is_none(),
            "D4"
        );
    }
}
