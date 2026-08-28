//! One capability-derived delivery and Environment-deferral decision.
//!
//! Session construction consumes these classifiers; prompt, mount, Native, and
//! ACP adapters never derive their own mode.

use super::*;

pub(super) fn published_skill_ids(
    snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> Option<std::collections::BTreeSet<String>> {
    snapshot.map(|snapshot| {
        snapshot
            .resolved_spec
            .plugin_config
            .agent
            .skills
            .iter()
            .map(|skill| skill.skill_id.clone())
            .collect()
    })
}

impl SharedHost {
    fn session_allows_agent_tool(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        tool: &str,
    ) -> bool {
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
            return agent.policy_for(tool).enabled;
        }
        published_snapshot.is_none_or(|snapshot| {
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .any(|descriptor| descriptor.id == tool)
        })
    }

    pub(crate) fn session_allows_filesystem_tools(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> bool {
        ["bash", "read", "write", "edit", "glob", "grep"]
            .into_iter()
            .any(|tool| self.session_allows_agent_tool(thread, published_snapshot, tool))
    }

    /// Direct Agent SDK-compatible repository Skill discovery follows the
    /// `read` capability rule, intentionally narrower than general filesystem
    /// capability. Managed Sessions consume only frozen binding bytes and do
    /// not call this discovery path.
    pub(crate) fn session_allows_repository_skill_discovery(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> bool {
        self.session_allows_agent_tool(thread, published_snapshot, "read")
    }

    pub(crate) fn select_content_delivery(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        frozen_skill_versions: Option<&[awaken_resource_contract::SkillVersion]>,
    ) -> Result<crate::session_slot::ManagedContentDelivery, HostError> {
        let managed_session = self
            .session_slots
            .read(thread, |slot| slot.session_dispatch)
            .unwrap_or(false);
        let selected_skills = published_skill_ids(published_snapshot);
        let managed_has_skill = if managed_session {
            // A cold reservation may precede Resource realization and therefore
            // carry neither a publication Skill selection nor a frozen Skill
            // vector yet. That means "no Skill projection", not an alternate
            // catalog. Once a publication selects a Skill, however, its exact
            // frozen bytes are mandatory.
            let frozen = frozen_skill_versions.unwrap_or_default();
            if let Some(missing) = selected_skills.as_ref().and_then(|selected| {
                selected.iter().find(|id| {
                    !frozen
                        .iter()
                        .any(|version| version.skill_id.as_str() == id.as_str())
                })
            }) {
                return Err(HostError::internal(format!(
                    "Managed Session Skill `{missing}` has no frozen version bytes"
                )));
            }
            frozen.iter().any(|version| {
                selected_skills
                    .as_ref()
                    .is_none_or(|selected| selected.contains(version.skill_id.as_str()))
            })
        } else {
            false
        };
        // Content delivery belongs to the physical Session, not to each
        // auxiliary/child Agent snapshot. Once the root projection chooses it,
        // a restricted Outcome grader or delegate must reuse that immutable
        // choice instead of trying to re-author the Session's mounts/tools.
        if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.content_delivery)
            .flatten()
        {
            if managed_has_skill
                && existing != crate::session_slot::ManagedContentDelivery::ManagedFilesystem
            {
                return Err(HostError::bad_request(
                    "Managed Agent Skills require filesystem progressive disclosure, but this Session disables every filesystem tool",
                ));
            }
            return Ok(existing);
        }
        let allows_filesystem = self.session_allows_filesystem_tools(thread, published_snapshot);
        if managed_has_skill && !allows_filesystem {
            return Err(HostError::bad_request(
                "Managed Agent Skills require filesystem progressive disclosure, but this Session disables every filesystem tool",
            ));
        }
        let delivery = if allows_filesystem {
            crate::session_slot::ManagedContentDelivery::ManagedFilesystem
        } else {
            crate::session_slot::ManagedContentDelivery::SemanticTools
        };
        if delivery == crate::session_slot::ManagedContentDelivery::SemanticTools
            && !managed_session
        {
            let frozen_requires_filesystem = frozen_skill_versions.is_some_and(|versions| {
                versions
                    .iter()
                    .any(crate::skills::version_requires_environment)
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
        let selected_skills = published_skill_ids(published_snapshot);
        let (managed_session, slot_requires, frozen_has_selected) = self
            .session_slots
            .read(thread, |slot| {
                let is_selected = |id: &str| {
                    selected_skills
                        .as_ref()
                        .is_none_or(|selected| selected.contains(id))
                };
                let frozen_has_selected = slot.skills.as_ref().is_some_and(|versions| {
                    versions
                        .iter()
                        .any(|version| is_selected(version.skill_id.as_str()))
                });
                let frozen_requires = slot.skills.as_ref().is_some_and(|versions| {
                    versions.iter().any(|version| {
                        is_selected(version.skill_id.as_str())
                            && crate::skills::version_requires_environment(version)
                    })
                });
                let mount_requires_environment =
                    |mount: &awaken_provisioning_contract::MountRequirement| {
                        filesystem_delivery
                            || !matches!(
                                mount.source,
                                awaken_provisioning_contract::MountSource::MemoryStore { .. }
                            )
                    };
                let baseline_requires = slot.baseline.as_ref().is_some_and(|baseline| {
                    baseline.mounts.iter().any(mount_requires_environment)
                        || !baseline.env.is_empty()
                });
                let slot_requires = !slot.delegates.is_empty()
                    || slot.resources.mounts.iter().any(mount_requires_environment)
                    || !slot.resources.repositories.is_empty()
                    || baseline_requires
                    || frozen_requires;
                (slot.session_dispatch, slot_requires, frozen_has_selected)
            })
            .unwrap_or((false, false, false));
        let workspace = self.thread_workspace(thread);
        let filesystem_skill_projection = filesystem_delivery
            && (frozen_has_selected
                || (!managed_session
                    && self
                        .skills
                        .has_selected_in(&workspace, selected_skills.as_ref())));
        slot_requires
            || filesystem_skill_projection
            || (!managed_session
                && self
                    .skills
                    .requires_environment_in(&workspace, selected_skills.as_ref()))
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
