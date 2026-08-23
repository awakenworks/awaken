//! One capability-derived delivery and Environment-deferral decision.
//!
//! Session construction consumes these classifiers; prompt, mount, Native, and
//! ACP adapters never derive their own mode.

use super::*;

impl SharedHost {
    fn session_allows_filesystem_tools(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> bool {
        const FILESYSTEM_TOOLS: [&str; 6] = ["bash", "read", "write", "edit", "glob", "grep"];
        let session_tools = self
            .session_slots
            .read(thread, |slot| slot.tools.clone())
            .flatten();
        let toolsets = session_tools
            .as_ref()
            .map(|tools| tools.toolsets.as_slice())
            .or_else(|| {
                published_snapshot.map(|snapshot| {
                    snapshot
                        .resolved_spec
                        .plugin_config
                        .agent
                        .toolsets
                        .as_slice()
                })
            });
        if let Some(agent) = toolsets.and_then(|toolsets| {
            toolsets.iter().find(|toolset| {
                matches!(
                    toolset.source,
                    awaken_runtime_contract::agent_bindings::ToolsetSource::Agent
                )
            })
        }) {
            return FILESYSTEM_TOOLS
                .iter()
                .any(|name| agent.policy_for(name).enabled);
        }
        published_snapshot.is_none_or(|snapshot| {
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .any(|descriptor| FILESYSTEM_TOOLS.contains(&descriptor.id.as_str()))
        })
    }

    pub(super) fn select_content_delivery(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        frozen_skill_versions: Option<&Vec<awaken_resource_contract::SkillVersion>>,
    ) -> Result<crate::session_slot::ManagedContentDelivery, HostError> {
        let delivery = if self.session_allows_filesystem_tools(thread, published_snapshot) {
            crate::session_slot::ManagedContentDelivery::ManagedFilesystem
        } else {
            crate::session_slot::ManagedContentDelivery::SemanticTools
        };
        if delivery == crate::session_slot::ManagedContentDelivery::SemanticTools {
            let frozen_requires_filesystem = frozen_skill_versions.is_some_and(|versions| {
                versions
                    .iter()
                    .any(crate::skills::version_requires_environment)
            });
            let selected_skills = published_snapshot.map(|snapshot| {
                snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .skills
                    .iter()
                    .map(|skill| skill.skill_id.clone())
                    .collect::<std::collections::BTreeSet<_>>()
            });
            if frozen_requires_filesystem
                || self.skills.requires_environment_in(
                    &self.thread_workspace(thread),
                    selected_skills.as_ref(),
                )
            {
                return Err(HostError::bad_request(
                    "a selected Skill requires filesystem tools, but this Session disables every filesystem tool",
                ));
            }
        }
        let conflicting = self
            .session_slots
            .read(thread, |slot| {
                slot.content_delivery
                    .is_some_and(|existing| existing != delivery)
            })
            .unwrap_or(false);
        if conflicting {
            return Err(HostError::bad_request(
                "Session content delivery cannot change after its first runtime projection",
            ));
        }
        self.session_slots
            .update(thread, |slot| slot.content_delivery = Some(delivery));
        Ok(delivery)
    }

    pub(super) fn session_has_local_environment_inputs(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> bool {
        let filesystem_delivery = self.session_allows_filesystem_tools(thread, published_snapshot);
        let slot_requires = self
            .session_slots
            .read(thread, |slot| {
                !slot.delegates.is_empty()
                    || slot.resources.mounts.iter().any(|mount| {
                        filesystem_delivery
                            || !matches!(
                                mount.source,
                                awaken_provisioning_contract::MountSource::MemoryStore { .. }
                            )
                    })
                    || !slot.resources.repositories.is_empty()
                    || slot.baseline.as_ref().is_some_and(|baseline| {
                        baseline.mounts.iter().any(|mount| {
                            filesystem_delivery
                                || !matches!(
                                    mount.source,
                                    awaken_provisioning_contract::MountSource::MemoryStore { .. }
                                )
                        }) || !baseline.env.is_empty()
                    })
                    || slot.skills.as_ref().is_some_and(|versions| {
                        versions
                            .iter()
                            .any(crate::skills::version_requires_environment)
                    })
            })
            .unwrap_or(false);
        let selected_skills = published_snapshot.map(|snapshot| {
            snapshot
                .resolved_spec
                .plugin_config
                .agent
                .skills
                .iter()
                .map(|skill| skill.skill_id.clone())
                .collect::<std::collections::BTreeSet<_>>()
        });
        let workspace = self.thread_workspace(thread);
        slot_requires
            || self
                .skills
                .requires_environment_in(&workspace, selected_skills.as_ref())
    }

    pub(super) fn can_defer_session_environment(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        has_published_delegates: bool,
    ) -> bool {
        // A Coordinator-only Host constructs the durable dispatch envelope but
        // never executes it. Creating an eager sandbox here would make the
        // Coordinator a second physical owner beside the registered Worker.
        if self.deployment.disable_local_pool {
            return true;
        }
        let slot_allows = self
            .session_slots
            .read(thread, |slot| {
                slot.deferred_executor.is_some()
                    && slot
                        .environment_projection
                        .as_ref()
                        .is_some_and(|projection| {
                            projection.provisioning
                                == awaken_session_contract::SandboxProvisioning::OnToolUse
                        })
            })
            .unwrap_or(false);
        let workspace = self.thread_workspace(thread);
        let published_backend_is_acp = published_snapshot
            .cloned()
            .or_else(|| {
                self.agent_publications.as_ref().and_then(|source| {
                    source.current(
                        &workspace,
                        &awaken_runtime_contract::snapshot::AgentId(
                            agent.unwrap_or("assistant").to_string(),
                        ),
                    )
                })
            })
            .is_some_and(|snapshot| {
                awaken_runtime_contract::resolved::Backend::from_ref(
                    &snapshot.resolved_spec.model_binding.backend_ref,
                )
                .is_acp()
            });
        let selected_backend_is_acp = self
            .session_slots
            .read(thread, |slot| slot.backend_ref.clone())
            .flatten()
            .is_some_and(|backend_ref| {
                awaken_runtime_contract::resolved::Backend::from_ref(&backend_ref).is_acp()
            });
        // Delegation itself is a Sandbox capability: the child must inherit the
        // exact parent environment and its lifecycle fence. Keep that fact in the
        // sole eager-vs-deferred classifier instead of accepting deferral here and
        // rejecting the same snapshot later while the Runtime is being wired.
        slot_allows
            && !self.session_has_local_environment_inputs(thread, published_snapshot)
            && !published_backend_is_acp
            && !selected_backend_is_acp
            && !has_published_delegates
    }
}
