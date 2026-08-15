//! Agent lifecycle admission for config authoring writes.

use awaken_agent_config::{AgentConfig, ConfigRegistry};

/// Prevent an archived or disabled Agent from being rewritten back into a
/// mutable draft. Publication and archive transitions remain owned by their
/// dedicated commands; ordinary draft writes may only preserve terminal state.
pub(super) async fn reject_archived_rewrite(
    registry: &dyn ConfigRegistry,
    config: &AgentConfig,
) -> Result<(), String> {
    let current = registry
        .get_config(&config.id)
        .await
        .map_err(|error| error.to_string())?;
    let invalid = current.as_ref().is_some_and(|stored| {
        use awaken_agent_config::AgentLifecycle::{Archived, Disabled, Published};
        match (stored.lifecycle(), config.lifecycle()) {
            (Published, _) | (Disabled, Archived) => false,
            (Disabled | Archived, _) => stored != config,
        }
    });
    if invalid {
        return Err(format!("agent `{}` is not published", config.id));
    }
    Ok(())
}
