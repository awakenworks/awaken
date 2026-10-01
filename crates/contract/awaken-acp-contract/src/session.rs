// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: The ACP Session MCP transport may carry process-private auth material;
// @formal reason: Debug redacts it and the exact delivery decision remains with the Worker.
//! Neutral ACP Session transport and launch-observation values.
//!
//! The protocol maps these values to its wire; the execution composition
//! projects them from one claim-owned Session. Neither operation has a second
//! catalog or permission authority here.

use serde::{Deserialize, Serialize};

/// A neutral MCP server for the ACP `session/new` `mcpServers` param. A stdio
/// child carries `command` and `args`; an HTTP endpoint carries `url`.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionMcpServer {
    pub name: String,
    /// Stdio transport: the child command + args (`url` is then `None`).
    pub command: Option<String>,
    pub args: Vec<String>,
    /// HTTP transport: the endpoint URL (`command` is then `None`).
    pub url: Option<String>,
    /// An `(name, value)` the CLI presents as an auth header (HTTP) or env var (stdio).
    /// α: a broker reference the gateway resolves; β: a raw secret on a trusted launch.
    /// `None` for an unauthenticated server.
    pub auth: Option<(String, String)>,
}

impl std::fmt::Debug for SessionMcpServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionMcpServer")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("url", &self.url)
            .field("auth", &self.auth.as_ref().map(|(name, _)| (name, "***")))
            .finish()
    }
}

/// Which stage of bringing an ACP agent online a lifecycle notification marks.
/// Ordered launch → initialize → ready so a UI renders a consistent progress
/// affordance. Adapter acquisition is a startup concern and never occurs in a
/// run. `Failed` is terminal; [`AcpLaunchEvent::detail`] says why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpLaunchStage {
    /// Spawning the agent process.
    Launching,
    /// ACP handshake in progress (`initialize` + `session/new`).
    Initializing,
    /// The agent is live: handshake done, about to accept the prompt turn.
    Ready,
    /// Bring-up failed at some stage (detail carries the cause).
    Failed,
}

/// One lifecycle notification for an ACP agent's bring-up — the neutral event a UI
/// renders as "launching… / ready". `detail` optionally carries human-facing
/// context such as a spawn error message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpLaunchEvent {
    pub stage: AcpLaunchStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AcpLaunchEvent {
    #[must_use]
    pub fn stage(stage: AcpLaunchStage) -> Self {
        Self {
            stage,
            detail: None,
        }
    }

    #[must_use]
    pub fn with_detail(stage: AcpLaunchStage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: Some(detail.into()),
        }
    }
}

/// The host's non-blocking observer for one ACP agent's bring-up. `scope`
/// identifies the Run/Thread whose UI receives the observation.
pub trait LaunchObserver: Send + Sync {
    fn on_launch(&self, scope: &str, event: &AcpLaunchEvent);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn session_mcp_debug_redacts_process_private_auth_material() {
        // Causes: C1 a process-private authentication value is present; C2 the
        // transport is an HTTP MCP route. Effects: E1 Debug retains route and
        // header name for diagnostics; E2 the value is always redacted. R1
        // C1+C2 -> name and `***`, never secret bytes. No process is launched.
        let server = SessionMcpServer {
            name: "docs".into(),
            command: None,
            args: Vec::new(),
            url: Some("https://mcp.example".into()),
            auth: Some(("Authorization".into(), "Bearer raw-secret".into())),
        };
        let debug = format!("{server:?}");
        assert!(debug.contains("Authorization"));
        assert!(!debug.contains("raw-secret"));
        assert!(debug.contains("***"));
    }

    #[test]
    fn launch_observation_preserves_json_shape_and_scope() {
        // Causes: C1 an event has no detail or C2 it carries a failure detail;
        // C3 an observer is installed for an exact scope. Effects: E1 absent
        // detail is omitted, E2 a detailed event round-trips, E3 that observer
        // receives the same stage/detail under only C3's scope. Decision table:
        // R1 C1 -> stage-only JSON; R2 C2 -> detail preserved; R3 C2+C3 -> one
        // scoped notification. The observer never grants launch authority.
        let bare = AcpLaunchEvent::stage(AcpLaunchStage::Launching);
        let value = serde_json::to_value(&bare).unwrap();
        assert!(value.get("detail").is_none());
        assert_eq!(value["stage"], "launching");

        let detailed = AcpLaunchEvent::with_detail(AcpLaunchStage::Failed, "spawn failed");
        let back: AcpLaunchEvent =
            serde_json::from_value(serde_json::to_value(&detailed).unwrap()).unwrap();
        assert_eq!(back, detailed);

        #[derive(Default)]
        struct RecordingObserver(Mutex<Vec<(String, AcpLaunchEvent)>>);
        impl LaunchObserver for RecordingObserver {
            fn on_launch(&self, scope: &str, event: &AcpLaunchEvent) {
                self.0.lock().unwrap().push((scope.into(), event.clone()));
            }
        }
        let observer = RecordingObserver::default();
        observer.on_launch("thread-42", &detailed);
        assert_eq!(
            *observer.0.lock().unwrap(),
            vec![("thread-42".into(), detailed)],
        );
    }
}
