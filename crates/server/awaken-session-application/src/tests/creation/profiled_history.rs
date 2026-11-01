//! Profiled creation must retain the same immutable presentation as Managed creation.
use super::*;
use awaken_session_contract::{SessionUpdateAgentConfiguration, SessionUpdateHistory};

/// Decorate the existing Profiled root fixture; the source changes its current
/// child only after returning that child's first real publication.
struct ObservedProfiledChildren {
    declarations: Vec<awaken_executable_agent_contract::ExecutableAgentDelegate>,
    first_skills: usize,
    later_skills: usize,
    missing: bool,
    root_present: bool,
    geo: Option<awaken_runtime_contract::agent_bindings::InferenceGeography>,
    reads: Mutex<Vec<(String, Option<u64>)>>,
}

impl ObservedProfiledChildren {
    fn new(pin: Option<u64>, first_skills: usize, later_skills: usize) -> Self {
        Self {
            declarations: vec![awaken_executable_agent_contract::ExecutableAgentDelegate {
                agent_id: "child".into(),
                source_revision: pin,
            }],
            first_skills,
            later_skills,
            missing: false,
            root_present: true,
            geo: None,
            reads: Default::default(),
        }
    }

    fn selected(
        &self,
        workspace: &str,
        id: &str,
        revision: Option<u64>,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        use awaken_executable_agent_contract::{
            ExecutableAgentDelegate, ExecutableAgentSessionProfile,
        };
        let mut reads = self.reads.lock().unwrap();
        reads.push((id.into(), revision));
        let occurrences = reads.iter().filter(|(agent, _)| agent == id).count();
        drop(reads);
        if workspace != "workspace" {
            return None;
        }
        if id == "profiled" && self.root_present {
            let mut root =
                awaken_executable_agent_contract::ExecutableAgentProfileSource::session_profile_in(
                    &ProfiledAgent { unavailable: false },
                    workspace,
                    id,
                )?;
            root.delegates = self.declarations.clone();
            return Some(root);
        }
        if id == "child" && !self.missing {
            let source_revision = revision.unwrap_or(if occurrences == 1 { 7 } else { 8 });
            return Some(ExecutableAgentSessionProfile {
                source_revision,
                skills: (0..if source_revision == 7 {
                    self.first_skills
                } else {
                    self.later_skills
                })
                    .map(|index| {
                        awaken_agent_contract::AgentSkillBinding::custom(format!("child-{index}"))
                    })
                    .collect(),
                inference: awaken_runtime_contract::agent_bindings::InferenceOptions {
                    inference_geo: self.geo,
                    ..Default::default()
                },
                delegates: ["nested", "nested", "child", "profiled"]
                    .into_iter()
                    .map(|agent_id| ExecutableAgentDelegate {
                        agent_id: agent_id.into(),
                        source_revision: None,
                    })
                    .collect(),
                ..Default::default()
            });
        }
        (id == "nested").then(|| ExecutableAgentSessionProfile {
            source_revision: 7,
            skills: vec![awaken_agent_contract::AgentSkillBinding::custom(
                "nested-skill",
            )],
            delegates: vec![ExecutableAgentDelegate {
                agent_id: "child".into(),
                source_revision: None,
            }],
            ..Default::default()
        })
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for ObservedProfiledChildren {
    fn session_profile_in(
        &self,
        workspace: &str,
        id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        self.selected(workspace, id, None)
    }

    fn session_profile_at_revision_in(
        &self,
        workspace: &str,
        id: &str,
        revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        self.selected(workspace, id, Some(revision))
    }
}

struct ProfiledCapabilities;

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for ProfiledCapabilities {
    fn capabilities(
        &self,
    ) -> Result<awaken_session_configuration_contract::AgentCapabilities, RunError> {
        Ok(awaken_session_configuration_contract::AgentCapabilities {
            delegates: vec!["child".into(), "missing".into()],
            ..Default::default()
        })
    }

    fn capabilities_for(
        &self,
        _session: &str,
    ) -> Result<awaken_session_configuration_contract::AgentCapabilities, RunError> {
        self.capabilities()
    }
}

#[tokio::test]
async fn profiled_without_a_profile_keeps_capability_selection_and_missing_child_defaults() {
    // R5 no root publication + capability-declared current child/missing child:
    // the original callback runs once per edge; existing child7 is presented,
    // missing child gets the original version-one default. An empty captured
    // Some would silently turn BOTH into defaults. No root profile still has
    // the original empty transitive Skill graph, not a newly imposed policy.
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let mut observed = ObservedProfiledChildren::new(None, 500, 0);
    observed.root_present = false;
    let source = Arc::new(observed);
    let selected = awaken_session_execution_contract::SessionExecutor::colocated(
        Arc::new(NoopRuntime),
        Arc::new(NoopMcpRealizer),
    );
    let mut roles = (*selected).clone();
    roles.capability = Arc::new(ProfiledCapabilities);
    let mut app = SessionApplicationBuilder::new(
        roles,
        SessionPersistence::new(repository, system_clock()),
        Arc::new(AdmissionEnvironment),
        {
            let probe = std::sync::Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
        },
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .unwrap();
    app.set_config_source(source.clone());
    let root = app
        .creation_commands
        .accept_profiled_session(profiled_session_command(
            "profiled-capability-defaults",
            None,
        ))
        .await
        .unwrap();
    let roster = SessionUpdateHistory::agent_configuration(&root)
        .unwrap()
        .multiagent
        .unwrap();
    let children = roster
        .members
        .iter()
        .map(|member| match member {
            awaken_session_configuration_contract::SessionAgentRosterMember::Agent(agent) => {
                (agent.agent_id.as_str(), agent.version, agent.skills.len())
            }
            _ => panic!("R5 capability child"),
        })
        .collect::<Vec<_>>();
    assert_eq!(children, [("child", 7, 500), ("missing", 1, 0)]);
    let reads = source.reads.lock().unwrap();
    for id in ["child", "missing"] {
        assert_eq!(
            reads
                .iter()
                .filter(|(agent, _)| agent == id)
                .cloned()
                .collect::<Vec<_>>(),
            [(id.to_owned(), None)]
        );
    }
    assert!(!reads.iter().any(|(id, _)| id == "nested"));
}

#[tokio::test]
async fn profiled_creation_keeps_pinned_and_current_occurrences_of_the_same_child() {
    // R6 actual Profiled entry: [(child,Some7),(child,current8)] must persist
    // [7,8], in order with both occurrences. The existing source decorator
    // advances after its first child read. This checks the composition edge,
    // not only the compiler table; the old captured first-id match emits [7,7].
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let mut observed = ObservedProfiledChildren::new(Some(7), 0, 0);
    observed
        .declarations
        .push(awaken_executable_agent_contract::ExecutableAgentDelegate {
            agent_id: "child".into(),
            source_revision: None,
        });
    let source = Arc::new(observed);
    let mut app = application(repository, Arc::new(AdmissionEnvironment));
    app.set_config_source(source.clone());
    let root = app
        .creation_commands
        .accept_profiled_session(profiled_session_command("profiled-mixed-child-pins", None))
        .await
        .unwrap();
    let roster = SessionUpdateHistory::agent_configuration(&root)
        .unwrap()
        .multiagent
        .unwrap();
    assert_eq!(
        roster
            .members
            .iter()
            .map(|member| match member {
                awaken_session_configuration_contract::SessionAgentRosterMember::Agent(agent) =>
                    agent.version,
                _ => panic!("R6 original ordinary child"),
            })
            .collect::<Vec<_>>(),
        [7, 8]
    );
    assert_eq!(
        root.frozen_baseline().unwrap().delegate_ids,
        ["child", "child"]
    );
    assert_eq!(
        source
            .reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id == "child")
            .cloned()
            .collect::<Vec<_>>(),
        [("child".into(), Some(7)), ("child".into(), None)]
    );
}

#[tokio::test]
async fn profiled_roster_and_skill_cap_share_the_original_selected_child() {
    // C1 acceptance/awaited realization x C2 current/exact child x C3 first
    // aggregate Skill count=500/501; the next current child reverses admission.
    // R1 first499+nested1=500, later500+nested1=501 => accept original7;
    // R2 first500+nested1=501, later0+nested1=1 => reject before root insert.
    // Both: one direct source read, original exact/current selector, and the
    // old child's Skills drive BOTH presentation and aggregate admission.
    // Repeated transitive edges and root/child cycles count each ID once by the
    // existing seen-set law; this task does not change it to count revisions.
    // Both create entries call that one validator directly. The removed
    // empty-capture forwarding method had no callers or runtime branch, so it
    // needs no mirrored test route; this table exercises actual admission.
    for accepted in [false, true] {
        for pin in [None, Some(7)] {
            for (first, later, allowed) in [(499, 500, true), (500, 0, false)] {
                let repository: Arc<dyn crate::SessionStore> = Arc::new(
                    awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                        .await
                        .unwrap(),
                );
                let source = Arc::new(ObservedProfiledChildren::new(pin, first, later));
                let mut app = application(repository.clone(), Arc::new(AdmissionEnvironment));
                app.set_config_source(source.clone());
                let command = profiled_session_command("captured-profiled", None);
                let result = if accepted {
                    app.creation_commands.accept_profiled_session(command).await
                } else {
                    app.creation_commands.create_profiled_session(command).await
                };
                if allowed {
                    let root = result.unwrap();
                    let config = SessionUpdateHistory::agent_configuration(&root).unwrap();
                    let roster = config.multiagent.unwrap();
                    let awaken_session_configuration_contract::SessionAgentRosterMember::Agent(
                        child,
                    ) = &roster.members[0]
                    else {
                        panic!("R1 original child")
                    };
                    assert_eq!(child.version, 7, "R1 first accepted child, never current8");
                    assert_eq!(child.skills.len(), first, "R1 same selected Skills");
                } else {
                    let SessionCreationError::Rejected(error) = result.unwrap_err() else {
                        panic!("R2 original admission failure")
                    };
                    assert_eq!(error.code, "invalid_request");
                    assert_eq!(
                        error.message,
                        "a session supports at most 500 skills across all agents"
                    );
                    assert!(
                        matches!(
                            repository.get(&"captured-profiled".into()).await,
                            Err(awaken_session_contract::SessionRepositoryError::NotFound)
                        ),
                        "R2 no insert, not an unavailable store"
                    );
                }
                let reads = source.reads.lock().unwrap();
                assert_eq!(
                    reads
                        .iter()
                        .filter(|(id, _)| id == "child")
                        .cloned()
                        .collect::<Vec<_>>(),
                    vec![("child".into(), pin)],
                    "R1/R2 direct selection is not repeated for counting"
                );
                assert_eq!(
                    reads.iter().filter(|(id, _)| id == "nested").count(),
                    1,
                    "R1/R2 transitive duplicates and cycles retain the original seen set"
                );
            }
        }
    }
}

#[tokio::test]
async fn profiled_capture_keeps_missing_child_errors_and_its_existing_geography_policy() {
    // R3 missing current/exact declared child => the SAME Profiled kind, code
    // and revision-specific text before any insert. R4 present child with Us
    // geography while root uses None => accept with inherited/equal override
    // model. Profiled had no Managed geography gate; sharing capture adds none.
    for pin in [None, Some(7)] {
        for missing in [false, true] {
            for model in [None, Some("published-model")] {
                let repository: Arc<dyn crate::SessionStore> = Arc::new(
                    awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                        .await
                        .unwrap(),
                );
                let mut observed = ObservedProfiledChildren::new(pin, 0, 0);
                observed.missing = missing;
                observed.geo =
                    Some(awaken_runtime_contract::agent_bindings::InferenceGeography::Us);
                let source = Arc::new(observed);
                let mut app = application(repository.clone(), Arc::new(AdmissionEnvironment));
                app.set_config_source(source);
                let result = app
                    .creation_commands
                    .accept_profiled_session(profiled_session_command(
                        "profiled-child-policy",
                        model,
                    ))
                    .await;
                if missing {
                    let SessionCreationError::Rejected(error) = result.unwrap_err() else {
                        panic!("R3 original Profiled error")
                    };
                    assert_eq!(error.kind, awaken_session_types::RunErrorKind::BadRequest);
                    assert_eq!(error.code, "invalid_request");
                    assert_eq!(
                        error.message,
                        format!(
                            "multiagent_unavailable: Agent `child` publication revision {pin:?} is unavailable"
                        )
                    );
                    assert!(matches!(
                        repository.get(&"profiled-child-policy".into()).await,
                        Err(awaken_session_contract::SessionRepositoryError::NotFound)
                    ));
                } else {
                    assert!(result.is_ok(), "R4 no new geography policy: {result:?}");
                }
            }
        }
    }
}

#[tokio::test]
async fn profiled_creation_records_the_selected_default_without_inventing_a_publication() {
    // D1 no Agent profile + accepted root; D2 no profile + realized root.
    // Both retain exactly the selected process model and Default provenance,
    // version-one presentation, and no invented publication revision.
    for accepted in [false, true] {
        let repository: Arc<dyn crate::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let app = application(repository.clone(), Arc::new(AdmissionEnvironment));
        let command = profiled_session_command("default-profiled-history", None);
        let root = if accepted {
            app.creation_commands.accept_profiled_session(command).await
        } else {
            app.creation_commands.create_profiled_session(command).await
        }
        .unwrap();
        let configuration = SessionUpdateHistory::agent_configuration(&root).unwrap();
        assert_eq!(
            configuration.model.model_id,
            root.frozen_baseline().unwrap().model
        );
        assert_eq!(
            configuration.model.source,
            awaken_session_configuration_contract::SessionModelSource::Default
        );
        assert_eq!(configuration.version, 1);
        assert_eq!(root.frozen_baseline().unwrap().agent_revision, None);
    }
}

#[tokio::test]
async fn profiled_creation_records_configuration_before_acceptance_and_mcp_update() {
    // Causes: C1 durable acceptance versus awaited realization; C2 exact
    // publication versus unversioned profile; C3 inherited versus explicit
    // equal model; C4 later MCP replacement. Effects: E1 the original insert
    // retains a complete configuration; E2 its identity matches the baseline;
    // E3 C4 records a replayable Agent update, never Unrecorded. Decision table:
    // cross C1 x C2 x C3 (8 rows); every realized row additionally exercises C4.
    // Existing creation tables own unavailable/different-model rejection and
    // exact replay; historical JSON tests forbid backfilling old acceptance.
    for accepted in [false, true] {
        for revision in [None, Some(7)] {
            for explicit_model in [false, true] {
                let repository: Arc<dyn crate::SessionStore> = Arc::new(
                    awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                        .await
                        .unwrap(),
                );
                let mut app = application(repository.clone(), Arc::new(AdmissionEnvironment));
                app.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
                let model = if revision.is_some() {
                    "historical-model"
                } else {
                    "published-model"
                };
                let mut command =
                    profiled_session_command("profiled-history", explicit_model.then_some(model));
                command.source_revision = revision;
                let root = if accepted {
                    app.creation_commands.accept_profiled_session(command).await
                } else {
                    app.creation_commands.create_profiled_session(command).await
                }
                .unwrap();
                let persisted = repository.get(&root.session_id).await.unwrap();
                let configuration = SessionUpdateHistory::agent_configuration(&persisted)
                    .expect("E1 Profiled creation retains its original Agent configuration");
                assert_eq!(configuration.agent_id, "profiled");
                assert_eq!(configuration.version, revision.unwrap_or(1), "E2");
                assert_eq!(configuration.model.model_id, model, "E2");
                assert_eq!(
                    configuration.model.source,
                    if explicit_model {
                        awaken_session_configuration_contract::SessionModelSource::Override
                    } else {
                        awaken_session_configuration_contract::SessionModelSource::Publication
                    },
                    "E2 selected model source remains exact"
                );
                assert_eq!(configuration.tools, persisted.tools, "E2");
                assert_eq!(
                    configuration.mcp_servers,
                    persisted.configured_mcp_servers(),
                    "E2"
                );
                assert_eq!(
                    persisted.frozen_baseline().unwrap().agent_revision,
                    revision,
                    "E2"
                );
                if !accepted {
                    let updated = app
                        .update_commands
                        .update_session(
                            &root.session_id,
                            SessionUpdateCommand {
                                title: None,
                                metadata: None,
                                budget: None,
                                tools: None,
                                mcp_update: Some(SessionMcpUpdate::PublicReplacement(Vec::new())),
                                idempotency_key: Some("profiled-mcp-update".into()),
                                request_fingerprint: "profiled-mcp-update".into(),
                                if_match: Some(persisted.revision),
                            },
                        )
                        .await
                        .unwrap();
                    let after = repository.get(&root.session_id).await.unwrap();
                    // Later MCP realization can advance the root; the retained
                    // update fact, not the latest root, owns the command CAS.
                    assert!(after.revision >= updated.command_revision);
                    let history = SessionUpdateHistory::for_session(&after).unwrap();
                    if !configuration.mcp_servers.is_empty() {
                        assert_eq!(
                            history.facts().last().unwrap().committed_revision(),
                            updated.command_revision
                        );
                        assert!(
                            matches!(
                                history.facts().last().unwrap().agent(),
                                Some(SessionUpdateAgentConfiguration::Recorded(_))
                            ),
                            "E3 MCP update must not poison later projection or cleanup"
                        );
                    }
                }
            }
        }
    }
}
