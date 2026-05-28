//! Shared `AgentSpec::validate_catalog` enforcement helper.
//!
//! Catalog validation is enforced at three sites:
//!
//! - the write path (`ConfigService::validate_payload` / overrides PATCH),
//! - the builtin seed path (`apply_builtin_seed`), and
//! - the runtime apply path (`ConfigRuntimeManager::validate_candidate`).
//!
//! All three apply identical policy: `Error` issues abort the operation,
//! `Warning` issues emit `tracing::warn!` and the operation proceeds. This
//! helper centralizes the loop so the three sites can't drift on policy.

use awaken_server_contract::AgentSpec;
use awaken_server_contract::registry_spec::IssueSeverity;

/// Run `AgentSpec::validate_catalog`: warnings emit `tracing::warn!`, the
/// raw error messages are returned (empty on success). The tracing target
/// is fixed because `tracing::warn!` requires a `&'static str` literal.
pub(crate) fn collect_catalog_errors(agent: &AgentSpec) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    for issue in agent.validate_catalog() {
        match issue.severity {
            IssueSeverity::Error => errors.push(issue.message),
            IssueSeverity::Warning => tracing::warn!(
                target: "awaken_server::agent_catalog",
                agent_id = %agent.id, field = issue.field, entry = %issue.entry,
                "{}", issue.message,
            ),
        }
    }
    errors
}

/// Apply `collect_catalog_errors` across a slice of agents, prefixing each
/// message with `agent.id`. Returns `Err(joined)` on any error, `Ok` otherwise.
pub(crate) fn check_catalog_errors(agents: &[AgentSpec]) -> Result<(), String> {
    let mut out: Vec<String> = Vec::new();
    for agent in agents {
        for msg in collect_catalog_errors(agent) {
            out.push(format!("{}: {msg}", agent.id));
        }
    }
    if out.is_empty() {
        Ok(())
    } else {
        Err(out.join("; "))
    }
}
