//! One active-active refresh boundary for Coordinator executable projections.

use std::sync::Arc;

use awaken_executable_agent_catalog::PostgresExecutableAgentRegistrar;
use awaken_executable_environment_catalog::PostgresExecutableEnvironmentRegistrar;
use awaken_session_contract::{
    ExecutableAgentProjectionRefresh, ExecutableEnvironmentProjectionRefresh,
    ExecutableProjectionRefresh,
};

struct AgentProjectionRefresh(Arc<PostgresExecutableAgentRegistrar>);

#[async_trait::async_trait]
impl ExecutableAgentProjectionRefresh for AgentProjectionRefresh {
    async fn refresh_agent_projection(&self) -> Result<(), String> {
        self.0
            .refresh_projection()
            .await
            .map_err(|error| error.to_string())
    }
}

struct EnvironmentProjectionRefresh(Arc<PostgresExecutableEnvironmentRegistrar>);

#[async_trait::async_trait]
impl ExecutableEnvironmentProjectionRefresh for EnvironmentProjectionRefresh {
    async fn refresh_environment_projection(&self) -> Result<(), String> {
        self.0
            .refresh_projection()
            .await
            .map_err(|error| error.to_string())
    }
}

fn pair(
    agents: Option<Arc<dyn ExecutableAgentProjectionRefresh>>,
    environments: Option<Arc<dyn ExecutableEnvironmentProjectionRefresh>>,
) -> Result<Option<Arc<ExecutableProjectionRefresh>>, &'static str> {
    match (agents, environments) {
        (Some(agents), Some(environments)) => Ok(Some(Arc::new(ExecutableProjectionRefresh::new(
            agents,
            environments,
        )))),
        (None, None) => Ok(None),
        _ => Err("executable projection refresh requires both Agent and Environment adapters"),
    }
}

/// Build the one process-wide active-active reconciliation handle. AllInOne
/// shares process-local catalogs and therefore returns no handle.
pub(crate) fn shared(
    agents: Option<Arc<PostgresExecutableAgentRegistrar>>,
    environments: Option<Arc<PostgresExecutableEnvironmentRegistrar>>,
) -> Result<Option<Arc<ExecutableProjectionRefresh>>, String> {
    pair(
        agents.map(|value| Arc::new(AgentProjectionRefresh(value)) as Arc<_>),
        environments.map(|value| Arc::new(EnvironmentProjectionRefresh(value)) as Arc<_>),
    )
    .map_err(str::to_owned)
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
    impl ExecutableAgentProjectionRefresh for RecordingRefresh {
        async fn refresh_agent_projection(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push(self.name);
            self.result.clone()
        }
    }

    #[async_trait::async_trait]
    impl ExecutableEnvironmentProjectionRefresh for RecordingRefresh {
        async fn refresh_environment_projection(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push(self.name);
            self.result.clone()
        }
    }

    fn recorder(
        calls: &Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
        result: Result<(), &str>,
    ) -> Arc<RecordingRefresh> {
        Arc::new(RecordingRefresh {
            calls: calls.clone(),
            name,
            result: result.map_err(str::to_string),
        })
    }

    #[tokio::test]
    async fn composite_refresh_is_ordered_fail_closed_and_absent_for_all_in_one() {
        // Causes: C1 Agent adapter is present/absent; C2 Environment adapter is
        // present/absent; C3 each admitted adapter refresh succeeds/fails.
        // Effects: E1 construct the sole ordered pair; E2 construct no gate for
        // process-local AllInOne; E3 reject a mixed process at startup; E4 call
        // Agent then Environment and succeed; E5 stop on the first failure.
        // Constraints: K1 the pair owns no cursor or cache; K2 no successful
        // gate can omit either domain; K3 Environment never runs after Agent
        // fails. Decision table: D1 present/present + TT=>E1,[A,E]/Ok(E4);
        // D2 present/present + F-=>E1,[A]/Err(E5); D3 present/present +
        // TF=>E1,[A,E]/Err(E5); D4 absent/absent=>None(E2); D5
        // present/absent=>Err(E3); D6 absent/present=>Err(E3).
        async fn exercise(
            agent: Result<(), &str>,
            environment: Result<(), &str>,
        ) -> (Vec<&'static str>, Result<(), String>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let refresh = pair(
                Some(recorder(&calls, "agent", agent)),
                Some(recorder(&calls, "environment", environment)),
            )
            .unwrap();
            let refresh = refresh.unwrap();
            let result = refresh.refresh().await.map(|_| ());
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
        assert!(pair(None, None).unwrap().is_none(), "D4");
        let calls = Arc::new(Mutex::new(Vec::new()));
        assert!(
            pair(Some(recorder(&calls, "agent", Ok(()))), None,).is_err(),
            "D5"
        );
        assert!(
            pair(None, Some(recorder(&calls, "environment", Ok(()))),).is_err(),
            "D6"
        );
        assert!(calls.lock().unwrap().is_empty(), "D5/D6 startup only");
    }
}
