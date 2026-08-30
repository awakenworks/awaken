//! Agent lifecycle admission for config authoring writes.

use awaken_agent_config::{AgentConfig, AgentLifecycle, ConfigRegistry, ConfigWrite};

pub(super) enum ExactSystemTransition {
    Archive,
    ReconciliationRevision,
}

/// Apply one system-owned CAS transition without passing historical bytes
/// through mutable authoring canonicalization. The candidate must be provably
/// identical to the current revision except for the exact lifecycle delta named
/// by `transition`; this port cannot edit or migrate permission policy.
pub(super) async fn put_exact_system_transition_if_revision(
    registry: &dyn ConfigRegistry,
    candidate: &AgentConfig,
    expected_revision: u64,
    transition: ExactSystemTransition,
) -> Result<ConfigWrite, String> {
    let Some(current) = registry
        .get_config_revision(&candidate.id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(ConfigWrite::Conflict {
            current_revision: None,
        });
    };
    if current.revision != expected_revision {
        return Ok(ConfigWrite::Conflict {
            current_revision: Some(current.revision),
        });
    }

    let valid = match transition {
        ExactSystemTransition::Archive => {
            if !matches!(
                current.config.lifecycle(),
                AgentLifecycle::Published | AgentLifecycle::Disabled
            ) {
                return Err(format!(
                    "agent `{}` exact archive source must be published or disabled",
                    candidate.id
                ));
            }
            let mut expected = current.config;
            expected.disabled_at = None;
            expected.archived_at = candidate.archived_at.clone();
            candidate.lifecycle() == AgentLifecycle::Archived && candidate == &expected
        }
        ExactSystemTransition::ReconciliationRevision => {
            if current.config.lifecycle() != AgentLifecycle::Published
                || candidate.lifecycle() != AgentLifecycle::Published
            {
                return Err(format!(
                    "agent `{}` exact reconciliation source must be published",
                    candidate.id
                ));
            }
            candidate == &current.config
        }
    };
    if !valid {
        return Err(format!(
            "agent `{}` exact system transition must be lifecycle-only",
            candidate.id
        ));
    }
    registry
        .put_config_if_revision(candidate, expected_revision)
        .await
        .map_err(|error| error.to_string())
}
