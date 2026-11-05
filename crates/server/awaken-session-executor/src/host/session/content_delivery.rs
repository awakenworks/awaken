// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Selects the one capability-derived filesystem or semantic delivery mode and rejects
// @formal reason: post-projection drift; tool and Skill authorities remain in their frozen Session inputs.
//! One capability-derived delivery and Environment-deferral decision.
//!
//! Session construction consumes these classifiers; prompt, mount, Native, and
//! ACP adapters never derive their own mode.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Immutable evidence available when choosing the Session-wide Skill delivery
/// profile. A reservation may use the aggregate-owned binding pins without
/// loading Resource Store bytes; every executable context must use the exact
/// materialized versions instead.
pub(crate) enum SkillDeliveryEvidence<'a> {
    Materialized(Option<&'a [awaken_resource_contract::SkillVersion]>),
    DeferredBindings(&'a [crate::session_resource::ResolvedSkillBinding]),
}

impl SkillDeliveryEvidence<'_> {
    fn materialized_versions(&self) -> Option<&[awaken_resource_contract::SkillVersion]> {
        match self {
            Self::Materialized(versions) => *versions,
            Self::DeferredBindings(_) => None,
        }
    }
}

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

impl crate::RunAssembly {
    fn session_allows_agent_tool(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        tool: &str,
    ) -> Result<bool, crate::realization_error::RealizationError> {
        let realizations = self.installed_sessions.realizations();
        let session_tools = realizations.frozen(thread)?.tools.clone();
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
                matches!(toolset.source, awaken_agent_contract::ToolsetSource::Agent)
            })
        }) {
            return Ok(agent.policy_for(tool).enabled);
        }
        Ok(published_snapshot.is_none_or(|snapshot| {
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .any(|descriptor| descriptor.id == tool)
        }))
    }

    pub(crate) fn session_allows_filesystem_tools(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<bool, crate::realization_error::RealizationError> {
        for tool in ["bash", "read", "write", "edit", "glob", "grep"] {
            if self.session_allows_agent_tool(thread, published_snapshot, tool)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Repository Skill discovery follows the exact `read` capability rule,
    /// intentionally narrower than general filesystem capability. The caller
    /// still chooses which already-realized repository roots are admitted for
    /// its execution profile.
    pub(crate) fn session_allows_repository_skill_discovery(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<bool, crate::realization_error::RealizationError> {
        self.session_allows_agent_tool(thread, published_snapshot, "read")
    }

    /// Select the only Environment-backed Skill roots for this Session.
    ///
    /// `None` excludes Environment discovery, `Some([])` retains the direct
    /// live-authored compatibility source, and `Some(roots)` freezes exactly
    /// those roots into the Session registry. Managed repository Skills come
    /// only from an already-realized Repository mount at Anthropic's fixed
    /// `.claude/skills` path; `plugin_config.skills_dir` remains a direct
    /// authored-workspace setting and cannot alter that contract.
    pub(crate) fn session_skill_source_roots(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        authored_skills_subdir: &str,
    ) -> Result<Option<Vec<String>>, SessionExecutionError> {
        let realizations = self.installed_sessions.realizations();
        let managed_session = self
            .installed_sessions
            .read_run(thread, |run| run.session_dispatch)?
            .unwrap_or(false);
        let read_enabled =
            self.session_allows_repository_skill_discovery(thread, published_snapshot)?;
        // A Managed cold recovery may carry its complete Session tool policy in
        // the durable installed Session without a catalog-backed publication snapshot. The
        // installed Session policy remains sufficient authority. Direct callers retain the
        // legacy rule that only a published Agent admits repository roots.
        let repository_discovery =
            read_enabled && (managed_session || published_snapshot.is_some());

        if managed_session && !repository_discovery {
            return Ok(None);
        }
        if !managed_session && !repository_discovery {
            return Ok(Some(Vec::new()));
        }

        let mut roots = realizations
            .read_realization(thread, |r| {
                r.staging()
                    .resources
                    .repositories
                    .iter()
                    .map(|repository| {
                        format!(
                            "{}/{}",
                            repository
                                .plan
                                .mount_path
                                .trim_start_matches('/')
                                .trim_end_matches('/'),
                            crate::skills::MANAGED_SKILLS_SUBDIR
                        )
                    })
                    .collect::<Vec<_>>()
            })?
            .unwrap_or_default();
        if !managed_session {
            roots.push(authored_skills_subdir.to_string());
        }
        roots.sort();
        roots.dedup();

        if managed_session && roots.is_empty() {
            Ok(None)
        } else {
            Ok(Some(roots))
        }
    }

    pub(crate) fn select_content_delivery(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        skill_evidence: SkillDeliveryEvidence<'_>,
    ) -> Result<crate::sandbox_layout::ManagedContentDelivery, SessionExecutionError> {
        let realizations = self.installed_sessions.realizations();
        let managed_session = self
            .installed_sessions
            .read_run(thread, |run| run.session_dispatch)?
            .unwrap_or(false);
        let selected_skills = published_skill_ids(published_snapshot);
        let managed_has_skill = if managed_session {
            // A cold reservation deliberately precedes Worker-owned Resource
            // realization. Its aggregate binding pins are enough to choose the
            // Session-wide delivery profile, but never to construct an executable
            // Skill registry. Every other open requires exact frozen bytes.
            let frozen = skill_evidence.materialized_versions().unwrap_or_default();
            if let Some(missing) = selected_skills.as_ref().and_then(|selected| {
                selected.iter().find(|id| {
                    let materialized = frozen
                        .iter()
                        .any(|version| version.skill_id.as_str() == id.as_str());
                    let deferred = matches!(
                        &skill_evidence,
                        SkillDeliveryEvidence::DeferredBindings(bindings)
                            if bindings
                                .iter()
                                .any(|binding| binding.skill_id.as_str() == id.as_str())
                    );
                    !materialized && !deferred
                })
            }) {
                return Err(SessionExecutionError::internal(format!(
                    "Managed Session Skill `{missing}` has neither frozen version bytes nor a deferred aggregate binding"
                )));
            }
            match &skill_evidence {
                SkillDeliveryEvidence::Materialized(_) => frozen.iter().any(|version| {
                    selected_skills
                        .as_ref()
                        .is_none_or(|selected| selected.contains(version.skill_id.as_str()))
                }),
                SkillDeliveryEvidence::DeferredBindings(bindings) => {
                    bindings.iter().any(|binding| {
                        selected_skills
                            .as_ref()
                            .is_none_or(|selected| selected.contains(binding.skill_id.as_str()))
                    })
                }
            }
        } else {
            false
        };
        // Content delivery belongs to the physical Session, not to each
        // auxiliary/child Agent snapshot. Once the root projection chooses it,
        // a restricted Outcome grader or delegate must reuse that immutable
        // choice instead of trying to re-author the Session's mounts/tools.
        if let Some(existing) = realizations
            .read_realization(thread, |r| r.staging().content_delivery)?
            .flatten()
        {
            if managed_has_skill
                && existing != crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem
            {
                return Err(SessionExecutionError::bad_request(
                    "Managed Agent Skills require filesystem progressive disclosure, but this Session disables every filesystem tool",
                ));
            }
            return Ok(existing);
        }
        let allows_filesystem = self.session_allows_filesystem_tools(thread, published_snapshot)?;
        if managed_has_skill && !allows_filesystem {
            return Err(SessionExecutionError::bad_request(
                "Managed Agent Skills require filesystem progressive disclosure, but this Session disables every filesystem tool",
            ));
        }
        let delivery = if allows_filesystem {
            crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem
        } else {
            crate::sandbox_layout::ManagedContentDelivery::SemanticTools
        };
        if delivery == crate::sandbox_layout::ManagedContentDelivery::SemanticTools
            && realizations
                .read_realization(thread, |r| {
                    r.staging().resources
                        .resource_prompts
                        .iter()
                        .any(crate::managed_input_layout::ResourcePromptProjection::requires_filesystem_delivery)
                })?
                .unwrap_or(false)
        {
            return Err(SessionExecutionError::bad_request(
                "MemoryStore instructions reference a filesystem path, but this Session disables filesystem delivery",
            ));
        }
        if delivery == crate::sandbox_layout::ManagedContentDelivery::SemanticTools
            && !managed_session
        {
            let frozen_requires_filesystem =
                skill_evidence
                    .materialized_versions()
                    .is_some_and(|versions| {
                        versions
                            .iter()
                            .any(crate::skills::version_requires_environment)
                    });
            if frozen_requires_filesystem
                || self.capabilities.tools.skills.requires_environment_in(
                    &self.capabilities.thread_workspace(thread)?,
                    selected_skills.as_ref(),
                )?
            {
                return Err(SessionExecutionError::bad_request(
                    "a selected Skill requires filesystem tools, but this Session disables every filesystem tool",
                ));
            }
        }
        realizations.update_realization(thread, |r| r.select_content_delivery(delivery))?;
        Ok(delivery)
    }

    pub(super) fn session_has_local_environment_inputs(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<bool, SessionExecutionError> {
        let filesystem_delivery =
            self.session_allows_filesystem_tools(thread, published_snapshot)?;
        let selected_skills = published_skill_ids(published_snapshot);
        let (managed_session, installed_session_requires, frozen_has_selected) =
            self.installed_sessions
                .read(thread, |installed| {
                    let is_selected = |id: &str| {
                        selected_skills
                            .as_ref()
                            .is_none_or(|selected| selected.contains(id))
                    };
                    let frozen_has_selected = installed
                        .realization
                        .staging()
                        .skills
                        .as_ref()
                        .is_some_and(|versions| {
                            versions
                                .iter()
                                .any(|version| is_selected(version.skill_id.as_str()))
                        });
                    // Managed attached bundles are immutable Resource bytes. The
                    // deferred `read` executor can disclose them before Hand exists;
                    // executing scripts still follows ordinary Sandbox demand.
                    // Direct semantic/fork compatibility retains its eager rule.
                    let frozen_requires = !installed.run.session_dispatch
                        && installed.realization.staging().skills.as_ref().is_some_and(
                            |versions| {
                                versions.iter().any(|version| {
                                    is_selected(version.skill_id.as_str())
                                        && crate::skills::version_requires_environment(version)
                                })
                            },
                        );
                    let mount_requires_environment =
                        |mount: &awaken_provisioning_contract::MountRequirement| {
                            filesystem_delivery
                                || !matches!(
                                    mount.source,
                                    awaken_provisioning_contract::MountSource::MemoryStore { .. }
                                )
                        };
                    let baseline_requires =
                        installed.inputs.baseline.as_ref().is_some_and(|baseline| {
                            baseline.mounts.iter().any(mount_requires_environment)
                                || !baseline.env.is_empty()
                        });
                    let installed_session_requires = !installed.inputs.delegates.is_empty()
                        || installed
                            .realization
                            .staging()
                            .resources
                            .mounts
                            .iter()
                            .any(mount_requires_environment)
                        || !installed
                            .realization
                            .staging()
                            .resources
                            .repositories
                            .is_empty()
                        || baseline_requires
                        || frozen_requires;
                    (
                        installed.run.session_dispatch,
                        installed_session_requires,
                        frozen_has_selected,
                    )
                })?
                .unwrap_or((false, false, false));
        let workspace = self.capabilities.thread_workspace(thread)?;
        let filesystem_skill_projection = !managed_session
            && filesystem_delivery
            && (frozen_has_selected
                || self
                    .capabilities
                    .tools
                    .skills
                    .has_selected_in(&workspace, selected_skills.as_ref())?);
        Ok(installed_session_requires
            || filesystem_skill_projection
            || (!managed_session
                && self
                    .capabilities
                    .tools
                    .skills
                    .requires_environment_in(&workspace, selected_skills.as_ref())?))
    }

    pub(super) fn can_defer_session_environment(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        has_published_delegates: bool,
        claimed_attempt: bool,
    ) -> Result<bool, SessionExecutionError> {
        let realizations = self.installed_sessions.realizations();
        // A Coordinator-only Host constructs the durable dispatch envelope but
        // never executes it. Creating an eager sandbox here would make the
        // Coordinator a second physical owner beside the registered Worker.
        if !self.runs_local_dispatch_pool() {
            return Ok(true);
        }
        // One deferred-environment decision owns both Coordinator and Worker
        // behavior. A claim-fenced inference-only auxiliary keeps Execution
        // purpose, but needs no undeclared physical substrate.
        let inference_only_claim = claimed_attempt
            && !has_published_delegates
            && published_snapshot.is_some_and(|snapshot| {
                crate::sandbox_layout::environment_free_inference(
                    snapshot,
                    awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor,
                )
            })
            && self
                .installed_sessions
                .read(thread, |installed| {
                    !installed.run.session_dispatch
                        && installed.inputs.environment_snapshot.is_none()
                        && installed
                            .realization
                            .staging()
                            .run_manifest(installed.run.session_dispatch)
                            .is_none()
                })?
                .unwrap_or(true);
        if inference_only_claim {
            return Ok(true);
        }
        let installed_session_allows = self
            .installed_sessions
            .read(thread, |installed| {
                installed.run.deferred_executor.is_some()
                    && installed
                        .inputs
                        .environment_projection
                        .as_ref()
                        .is_some_and(|projection| {
                            projection.provisioning
                                == awaken_provisioning_contract::SandboxProvisioning::OnToolUse
                        })
            })?
            .unwrap_or(false);
        let workspace = self.capabilities.thread_workspace(thread)?;
        let published_backend_is_acp = published_snapshot
            .cloned()
            .or_else(|| {
                self.capabilities
                    .agent
                    .agent_publications
                    .as_ref()
                    .and_then(|source| {
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
                    snapshot.resolved_spec.model_binding.identity().2,
                )
                .is_acp()
            });
        let selected_backend_is_acp = realizations
            .frozen(thread)?
            .backend_ref
            .clone()
            .is_some_and(|backend_ref| {
                awaken_runtime_contract::resolved::Backend::from_ref(&backend_ref).is_acp()
            });
        // Delegation itself is a Sandbox capability: the child must inherit the
        // exact parent environment and its lifecycle fence. Keep that fact in the
        // sole eager-vs-deferred classifier instead of accepting deferral here and
        // rejecting the same snapshot later while the Runtime is being wired.
        Ok(installed_session_allows
            && !self.session_has_local_environment_inputs(thread, published_snapshot)?
            && !published_backend_is_acp
            && !selected_backend_is_acp
            && !has_published_delegates)
    }
}
