// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: This shared read predicate selects revisions without mutating publication state.
//! One visibility rule for Managed and complete-configuration revision reads.

use awaken_agent_config::AgentLifecycle;

pub(crate) fn revision_is_visible(
    lifecycle: AgentLifecycle,
    published: bool,
    current: bool,
) -> bool {
    match lifecycle {
        AgentLifecycle::Published => published,
        AgentLifecycle::Disabled | AgentLifecycle::Archived => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_requires_a_publication_or_the_current_terminal_revision() {
        // DT-VIS: enabled config + durable publication -> visible regardless
        // of head position; enabled draft without publication -> hidden.
        // Disabled/archived -> visible only at current head. An inconsistent
        // publication flag cannot promote a historical lifecycle snapshot.
        for lifecycle in [
            AgentLifecycle::Published,
            AgentLifecycle::Disabled,
            AgentLifecycle::Archived,
        ] {
            for published in [false, true] {
                for current in [false, true] {
                    let expected = if lifecycle == AgentLifecycle::Published {
                        published
                    } else {
                        current
                    };
                    assert_eq!(revision_is_visible(lifecycle, published, current), expected);
                }
            }
        }
    }
}
