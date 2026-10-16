//! Retained Session updates use the original root-mutation SQL conformance owner.
use super::*;
use awaken_session_budget_contract::{
    ManagedBudgetUsageCursor, ManagedListPriceSnapshot, SessionBudgetState,
};
use awaken_session_configuration_contract::{
    SessionAgentConfiguration, SessionAgentRoster, SessionAgentRosterMember, SessionModelChoice,
    SessionModelSource, SessionToolConfiguration,
};
use awaken_session_state_contract::{SessionAggregate, SessionUpdateHistory};
use awaken_store_conformance::session_store::record_update;

fn pricing() -> ManagedListPriceSnapshot {
    ManagedListPriceSnapshot {
        snapshot_id: "update-history-price".into(),
        version: 1,
        effective_at_unix_ms: 0,
        arithmetic_version: 1,
        model_rates: Default::default(),
        runtime_rates: Default::default(),
        fingerprint: "update-history-price-fingerprint".into(),
    }
}

pub(crate) fn fresh(id: &str) -> PersistedSession {
    let original = sample(id);
    let state = PersistedSession::frozen_with_budget(
        id,
        original.frozen_baseline().unwrap().clone(),
        original.resources.clone(),
        original.mcp.clone(),
        original.title.clone(),
        original.metadata.clone(),
        original.tools.clone(),
        SessionBudgetState::active(10, pricing()),
    );
    let mut root = SessionAggregate::try_from(state).unwrap();
    let baseline = root.frozen_baseline().unwrap();
    let mut configuration = SessionAgentConfiguration {
        agent_id: baseline.agent_id.clone(),
        version: 3,
        model: SessionModelChoice {
            model_id: baseline.model.clone(),
            inference: Default::default(),
            source: SessionModelSource::Publication,
        },
        name: "Accepted coder".into(),
        description: Some("Original selected presentation".into()),
        system: Some("Original accepted system".into()),
        tools: Default::default(),
        mcp_servers: Vec::new(),
        skills: vec![awaken_agent_contract::AgentSkillBinding::custom(
            "accepted-skill",
        )],
        multiagent: None,
    };
    configuration.multiagent = Some(SessionAgentRoster {
        members: vec![
            SessionAgentRosterMember::Agent(Box::new(configuration.clone())),
            SessionAgentRosterMember::Advisor("accepted-advisor".into()),
        ],
    });
    root.record_initial_agent_configuration(Box::new(configuration))
        .unwrap();
    // The existing store sample supplies an idle configuration fixture. Keep
    // that phase after the real fresh-root recording operation.
    let mut state = root.into_snapshot();
    state.execution = SessionExecutionState::Idle;
    state
}

fn legacy(id: &str) -> PersistedSession {
    // Historical absence is represented by genuinely omitted wire fields, not
    // a fresh empty history promoted to evidence about old updates.
    let mut wire = serde_json::to_value(fresh(id)).unwrap();
    for key in ["update_history", "initial_agent_configuration"] {
        wire.as_object_mut().unwrap().remove(key);
    }
    serde_json::from_value(wire).unwrap()
}

fn mutation(candidate: PersistedSession, key: &str) -> SessionMutation {
    let payload = SessionMutationPayload::Replace(candidate);
    SessionMutation {
        expected_revision: match &payload {
            SessionMutationPayload::Replace(root) => root.revision,
            SessionMutationPayload::Delete(_) => unreachable!(),
        },
        idempotency: IdempotencyRecord {
            key: key.into(),
            payload_hash: payload.stable_hash(),
        },
        lifecycle_facts: vec![fact(key, payload.session_id().as_str(), "session.updated")],
        payload,
    }
}

fn authoring_candidate(before: &PersistedSession, field: &str) -> PersistedSession {
    let mut root = SessionAggregate::try_from(before.clone()).unwrap();
    match field {
        "title" => root.configuration().unwrap().set_title(None),
        "metadata" => root
            .configuration()
            .unwrap()
            .patch_metadata(&BTreeMap::from([
                ("team".into(), None),
                ("updated".into(), Some("value".into())),
            ])),
        "tools" => root
            .configuration()
            .unwrap()
            .set_tools(SessionToolConfiguration {
                toolsets: Vec::new(),
                client_tools: vec![awaken_agent_contract::ClientToolDescriptor {
                    name: "accepted-client".into(),
                    description: "Exact client tool".into(),
                    input_schema: serde_json::json!({"type": "object", "required": ["key"]}),
                }],
            })
            .unwrap(),
        "mcp" => {
            root.mcp()
                .request_full_replacement(Vec::new(), None)
                .unwrap();
        }
        "budget" => root.budget().update_limit(Some(20)).unwrap(),
        _ => unreachable!(),
    }
    root.into_snapshot()
}

async fn reject_without_publication<R: awaken_session_repository_contract::SessionStore>(
    repo: &R,
    before: &PersistedSession,
    candidate: PersistedSession,
    key: &str,
) {
    let outbox = repo.pending_lifecycle().await.unwrap();
    assert!(matches!(
        repo.commit("ws_a", mutation(candidate, key)).await,
        Err(SessionRepositoryError::InvalidMutation(_))
    ));
    assert_eq!(repo.get(&before.session_id).await.unwrap(), *before);
    assert_eq!(
        repo.idempotency_receipt(&before.session_id, key)
            .await
            .unwrap(),
        None
    );
    assert_eq!(repo.pending_lifecycle().await.unwrap(), outbox);
}

pub(super) async fn replacement_table<R: awaken_session_repository_contract::SessionStore>(
    repo: &R,
    prefix: &str,
) {
    // SQL-H1/H2: C1=fresh-known/legacy-absent; C2=one of five authoring
    // fields changes; C3=fact omitted/recorded. The 2x5 rows first require
    // omitted -> InvalidMutation with exact root/receipt/outbox stutter, then
    // recorded under the SAME key -> Applied, exact one fact at next revision.
    // Correcting a rejected candidate cannot consume its key. H3 receipt match
    // beats invalid current-root history; H4 altered hash remains mismatch.
    // Both real SQL adapters call this from the original mutation table.
    for historical in [false, true] {
        for field in ["title", "metadata", "tools", "mcp", "budget"] {
            let id = format!("{prefix}-history-{historical}-{field}");
            let seed = if historical { legacy(&id) } else { fresh(&id) };
            let before = create_fixture(repo, "ws_a", seed, Vec::new()).await;
            let candidate = authoring_candidate(&before, field);
            let key = format!("history:{id}");
            reject_without_publication(repo, &before, candidate.clone(), &key).await;
            let mut expected = record_update(&before, candidate, 0).into_snapshot();
            let command = mutation(expected.clone(), &key);
            assert_eq!(
                repo.commit("ws_a", command.clone()).await.unwrap(),
                SessionMutationResult::Applied {
                    new_revision: SessionRevision(2)
                }
            );
            expected.revision = SessionRevision(2);
            assert_eq!(repo.get(&before.session_id).await.unwrap(), expected);
            let history = SessionUpdateHistory::for_session(&expected).unwrap();
            assert_eq!(
                history.known_after_revision(),
                SessionRevision(u64::from(historical))
            );
            assert_eq!(history.facts().len(), 1);
            assert_eq!(history.facts()[0].committed_revision(), SessionRevision(2));
            assert_eq!(
                history.facts()[0].recorded_at_unix_ms(),
                expected.updated_at_unix_ms
            );
            assert_eq!(
                history.facts()[0].observed_runtime().source_commit_cursor,
                0
            );
            assert_fact_payload(&expected, field, historical);
            assert!(
                repo.pending_lifecycle()
                    .await
                    .unwrap()
                    .contains(&command.lifecycle_facts[0])
            );
            let mut replay = command.clone();
            // Structurally valid history-free candidate, invalid against the
            // durable root; validation-before-SQL must remain intact.
            if let SessionMutationPayload::Replace(root) = &mut replay.payload {
                let mut wire = serde_json::to_value(&*root).unwrap();
                wire.as_object_mut().unwrap().remove("update_history");
                *root = serde_json::from_value(wire).unwrap();
            }
            assert!(replay.validate().is_ok());
            assert_eq!(
                repo.commit("ws_a", replay.clone()).await.unwrap(),
                SessionMutationResult::Replayed {
                    new_revision: SessionRevision(2)
                }
            );
            replay.idempotency.payload_hash = "different-request".into();
            assert_eq!(
                repo.commit("ws_a", replay).await.unwrap(),
                SessionMutationResult::IdempotencyMismatch
            );
            assert_eq!(repo.get(&before.session_id).await.unwrap(), expected);
            repo.complete_lifecycle(&key).await.unwrap();
            history_rewrite_table(repo, &expected, &key).await;
        }
    }
    runtime_only_table(repo, prefix).await;
}

fn assert_fact_payload(root: &PersistedSession, field: &str, historical: bool) {
    // Independent effect oracle: exactly the selected input field is marked;
    // optional clear is preserved, and Agent history records the full accepted
    // presentation only when the original creation actually recorded it.
    use awaken_session_state_contract::{SessionFieldUpdate, SessionUpdateAgentConfiguration};
    let fact = &SessionUpdateHistory::for_session(root).unwrap().facts()[0];
    assert_eq!(
        [
            fact.changes().title(),
            fact.changes().metadata(),
            fact.changes().tools(),
            fact.changes().mcp(),
            fact.changes().budget(),
        ],
        [
            field == "title",
            field == "metadata",
            field == "tools",
            field == "mcp",
            field == "budget",
        ]
    );
    match field {
        "title" => assert_eq!(fact.title(), Some(&SessionFieldUpdate::Clear)),
        "metadata" => assert_eq!(
            fact.metadata().cloned(),
            Some(BTreeMap::from([("updated".into(), "value".into())]))
        ),
        "budget" => assert_eq!(fact.budget(), Some(&SessionFieldUpdate::Replace(20))),
        "tools" | "mcp" => {
            if historical {
                assert_eq!(
                    fact.agent(),
                    Some(&SessionUpdateAgentConfiguration::Unrecorded)
                );
            } else {
                let Some(SessionUpdateAgentConfiguration::Recorded(configuration)) = fact.agent()
                else {
                    panic!("fresh creation keeps its complete accepted presentation");
                };
                assert_eq!(configuration.agent_id, "coder");
                assert_eq!(configuration.version, 3);
                assert_eq!(configuration.model.model_id, "kimi-k2");
                assert_eq!(configuration.model.source, SessionModelSource::Publication);
                assert_eq!(configuration.name, "Accepted coder");
                assert_eq!(
                    configuration.system.as_deref(),
                    Some("Original accepted system")
                );
                assert_eq!(configuration.skills[0].skill_id, "accepted-skill");
                assert_eq!(configuration.multiagent.as_ref().unwrap().members.len(), 2);
                assert_eq!(configuration.tools, root.tools);
                assert_eq!(configuration.mcp_servers, root.configured_mcp_servers());
            }
        }
        _ => unreachable!(),
    }
}

async fn history_rewrite_table<R: awaken_session_repository_contract::SessionStore>(
    repo: &R,
    before: &PersistedSession,
    prefix: &str,
) {
    // SQL-H5: immutable fact/prefix/coverage/initial value cannot be rewritten,
    // erased or double-appended. Each fresh key rejects atomically. The wire
    // fixture deliberately constructs states the aggregate would not author;
    // no production history setter or alternate recording method is introduced.
    let original = serde_json::to_value(before).unwrap();
    for fault in ["erase", "rewrite", "coverage", "double", "initial"] {
        let mut wire = original.clone();
        match fault {
            "erase" => {
                wire.as_object_mut().unwrap().remove("update_history");
            }
            "rewrite" => {
                wire["update_history"]["facts"][0]["recorded_at_unix_ms"] = serde_json::json!(9);
            }
            "coverage" => {
                wire["update_history"]["known_after_revision"] = serde_json::json!(2);
            }
            "double" => {
                let fact = wire["update_history"]["facts"][0].clone();
                wire["update_history"]["facts"]
                    .as_array_mut()
                    .unwrap()
                    .push(fact);
            }
            "initial" => {
                // Legacy has no initial presentation; adding it still cannot
                // turn an old absent creation fact into newly asserted history.
                wire["initial_agent_configuration"] = serde_json::to_value(
                    SessionUpdateHistory::agent_configuration(&fresh("initial-template")).unwrap(),
                )
                .unwrap();
                wire["initial_agent_configuration"]["name"] = serde_json::json!("rewritten");
            }
            _ => unreachable!(),
        }
        reject_without_publication(
            repo,
            before,
            serde_json::from_value(wire).unwrap(),
            &format!("{prefix}:{fault}"),
        )
        .await;
    }
}

async fn runtime_only_table<R: awaken_session_repository_contract::SessionStore>(
    repo: &R,
    prefix: &str,
) {
    // SQL-H6/H7: fresh/legacy x MCP realization/retry or cumulative budget usage.
    // Same desired configuration/limit -> Applied with history exactly retained,
    // while real phase/accounting values advance. No authoring fact is invented.
    // H8 a no-op root replacement likewise preserves fresh/unknown history.
    use awaken_session_configuration_contract::mcp::McpRealizationClaim;
    for historical in [false, true] {
        let id = format!("{prefix}-runtime-{historical}");
        let seed = if historical { legacy(&id) } else { fresh(&id) };
        let mut before = create_fixture(repo, "ws_a", seed, Vec::new()).await;
        for phase in ["mcp", "mcp_retry", "budget", "noop"] {
            let mut root = SessionAggregate::try_from(before.clone()).unwrap();
            if phase == "mcp" {
                let generation = root.mcp.attachments[0].generation.clone();
                root.mcp()
                    .claim_realization(
                        &generation,
                        McpRealizationClaim {
                            realization_id: "original-realization".into(),
                            runtime_incarnation: "worker/boot".into(),
                            lease_epoch: 1,
                            lease_expires_at_unix_ms: 30_000,
                            stage_idempotency_key: "original-stage".into(),
                        },
                    )
                    .unwrap();
                root.mcp()
                    .fail_realization(&generation, "original-realization", "retry")
                    .unwrap();
            } else if phase == "mcp_retry" {
                let attachment = &root.mcp.attachments[0];
                let draft = McpAttachmentDraft {
                    name: attachment.name.clone(),
                    target: attachment.target.clone(),
                    prompts_as_skills: attachment.prompts_as_skills,
                    credential: attachment.credential.clone(),
                    origin: attachment.origin,
                };
                root.mcp()
                    .request_full_replacement(vec![draft], None)
                    .unwrap();
                assert_eq!(root.mcp.desired_fingerprint, before.mcp.desired_fingerprint);
                assert_eq!(root.mcp.attachments.last().unwrap().generation.version(), 2);
            } else if phase == "budget" {
                root.budget()
                    .reconcile_cumulative_usage(ManagedBudgetUsageCursor {
                        active_seconds: 5,
                        ..Default::default()
                    })
                    .unwrap();
            }
            let command = root.compile_mutation(
                "ws_a",
                IdempotencyRecord {
                    key: format!("{id}:{phase}"),
                    payload_hash: format!("request:{id}:{phase}"),
                },
                Vec::new(),
                100,
            );
            assert_eq!(
                repo.commit("ws_a", command).await.unwrap(),
                SessionMutationResult::Applied {
                    new_revision: SessionRevision(before.revision.0 + 1)
                }
            );
            let expected = root
                .into_applied_snapshot(SessionRevision(before.revision.0 + 1))
                .unwrap();
            assert_eq!(
                SessionUpdateHistory::for_session(&expected),
                SessionUpdateHistory::for_session(&before)
            );
            assert_eq!(repo.get(&before.session_id).await.unwrap(), expected);
            before = expected;
        }
        assert_eq!(before.budget.usage_cursor().unwrap().active_seconds, 5);
    }
}

async fn retained_cold_fixtures<R: awaken_session_repository_contract::SessionStore>(
    repo: &R,
    prefix: &str,
) -> Vec<PersistedSession> {
    // SQL-H9/H10: fresh vs genuinely omitted historical fields, two real updates
    // separated by no-op persistence. Fresh initial configuration (including
    // nested roster/skills) remains immutable; legacy stays absent until its
    // first real update, whose known_after is that CURRENT revision, not zero.
    // Full root equality after reconnect is the persistence oracle. The exact
    // retained revision/payload/runtime anchor are stable-ID inputs; this store
    // test intentionally does not copy or reimplement the event-ID encoder.
    let unrecorded_id = format!("{prefix}-unrecorded");
    let unrecorded = create_fixture(repo, "ws_a", legacy(&unrecorded_id), Vec::new()).await;
    assert!(SessionUpdateHistory::for_session(&unrecorded).is_none());
    let mut expected = vec![unrecorded];
    for historical in [false, true] {
        let id = format!("{prefix}-{historical}");
        let seed = if historical { legacy(&id) } else { fresh(&id) };
        let mut before = create_fixture(repo, "ws_a", seed, Vec::new()).await;
        for _ in 0..2 {
            let no_op = before.clone();
            before = awaken_store_conformance::session_store::replace_session(
                repo,
                "ws_a",
                no_op,
                &format!("{id}:noop:{}", before.revision.0),
                Vec::new(),
            )
            .await;
        }
        assert_eq!(
            SessionUpdateHistory::for_session(&before).is_none(),
            historical
        );
        let known_after = before.revision;
        let initial = serde_json::to_value(&before)
            .unwrap()
            .get("initial_agent_configuration")
            .cloned();
        for field in ["title", "tools"] {
            let candidate = authoring_candidate(&before, field);
            let mut candidate =
                record_update(&before, candidate, 93 + u64::from(field == "tools")).into_snapshot();
            let command = mutation(candidate.clone(), &format!("{id}:{field}"));
            let next = SessionRevision(before.revision.0 + 1);
            assert_eq!(
                repo.commit("ws_a", command.clone()).await.unwrap(),
                SessionMutationResult::Applied { new_revision: next }
            );
            candidate.revision = next;
            repo.complete_lifecycle(&command.lifecycle_facts[0].id)
                .await
                .unwrap();
            before = candidate;
        }
        let history = SessionUpdateHistory::for_session(&before).unwrap();
        assert_eq!(
            history.known_after_revision(),
            if historical {
                known_after
            } else {
                SessionRevision(0)
            }
        );
        assert_eq!(history.facts().len(), 2);
        assert_eq!(history.facts()[0].committed_revision(), SessionRevision(4));
        assert_eq!(history.facts()[1].committed_revision(), SessionRevision(5));
        assert_eq!(
            history.facts()[0].observed_runtime().source_commit_cursor,
            93
        );
        assert_eq!(
            history.facts()[1].observed_runtime().source_commit_cursor,
            94
        );
        assert_eq!(
            serde_json::to_value(&before)
                .unwrap()
                .get("initial_agent_configuration")
                .cloned(),
            initial
        );
        expected.push(before);
    }
    expected
}

async fn assert_retained_cold_fixtures<R: awaken_session_repository_contract::SessionStore>(
    repo: &R,
    expected: Vec<PersistedSession>,
) {
    for root in expected {
        let actual = repo.get(&root.session_id).await.unwrap();
        assert_eq!(actual, root, "SQL-H9/H10 complete cold aggregate");
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(root).unwrap()
        );
    }
}

#[tokio::test]
async fn session_update_history_survives_sqlite_close_and_reopen() {
    // SQL-H9/H10 use an actual persistent file, explicit connection close and a
    // new repository constructor. In-memory equality cannot prove this effect.
    let (_directory, path) = temp_database("session-updates.db");
    let repo = SqliteManagedSessionRepository::open(&path).await.unwrap();
    let expected = retained_cold_fixtures(&repo, "sqlite-cold-history").await;
    close_repository(repo).await;
    let reopened = SqliteManagedSessionRepository::open(&path).await.unwrap();
    assert_retained_cold_fixtures(&reopened, expected).await;
    close_repository(reopened).await;
}

#[tokio::test]
async fn session_update_history_survives_postgres_pool_reconnect() {
    // Same SQL-H9/H10 table over the real backend. Existing required-Postgres
    // admission makes configured/unreachable DB a failure, never a fake pass.
    // The original scoped-schema mechanism isolates these rows; no new runner.
    use sqlx::Executor;
    let Some((url, admin)) = postgres_test_admin().await else {
        return;
    };
    let schema = "t_session_update_history";
    admin
        .execute("DROP SCHEMA IF EXISTS t_session_update_history CASCADE")
        .await
        .unwrap();
    admin
        .execute("CREATE SCHEMA t_session_update_history")
        .await
        .unwrap();
    let scoped_url = scoped_postgres_url(&url, schema);
    let pool = sqlx::postgres::PgPool::connect(&scoped_url).await.unwrap();
    let repo = PostgresManagedSessionRepository::with_pool(pool)
        .await
        .unwrap();
    let expected = retained_cold_fixtures(&repo, "pg-cold-history").await;
    repo.pool.close().await;
    let pool = sqlx::postgres::PgPool::connect(&scoped_url).await.unwrap();
    let reopened = PostgresManagedSessionRepository::with_pool(pool)
        .await
        .unwrap();
    assert_retained_cold_fixtures(&reopened, expected).await;
    reopened.pool.close().await;
    admin
        .execute("DROP SCHEMA t_session_update_history CASCADE")
        .await
        .unwrap();
    admin.close().await;
}
