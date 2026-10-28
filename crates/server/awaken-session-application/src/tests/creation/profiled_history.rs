//! Profiled creation must retain the same immutable presentation as Managed creation.
use super::*;
use awaken_session_state_contract::{SessionUpdateAgentConfiguration, SessionUpdateHistory};

#[tokio::test]
async fn profiled_creation_records_the_selected_default_without_inventing_a_publication() {
    // D1 no Agent profile + accepted root; D2 no profile + realized root.
    // Both retain exactly the selected process model and Default provenance,
    // version-one presentation, and no invented publication revision.
    for accepted in [false, true] {
        let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
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
                let repository: Arc<dyn awaken_session_repository_contract::SessionStore> =
                    Arc::new(
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
