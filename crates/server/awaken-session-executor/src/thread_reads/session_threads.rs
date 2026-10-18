// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: read-only Thread coordination queries split out of host/coordination.rs.
// @formal reason: Dynamic path: every query reads the parent Session's committed snapshot through the same
// @formal reason: authority's narrower read port, with no cache and no write path, so a child recovery or
// @formal reason: usage read cannot rebuild history or observe a boundary the parent has not committed.
// @formal reason: Evidence: session.multiagent.thread_authority plus this module's own tests
// @formal reason: (thread_usage_reads_authoritative_state_without_rebuilding_history,
// @formal reason: child_recovery_snapshot_reuses_the_parent_commit_consistency_boundary); this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Read-only coordination queries through the parent Session commit boundary.
//!
//! Full recovery still owns cross-region prefixes. State-only queries reuse the
//! same authority's narrower read port; this module has no cache or write path.

use super::*;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::thread::read::lifecycle::{RunLifecycleCursor, RunLifecycleFeed as _};
use awaken_session_realization::peers::EnvironmentPeer as _;
use awaken_session_execution_contract::CoordinatedThreadLink;

const MAX_LIFECYCLE_PAGE: usize = 1_024;

impl ThreadReads {
    pub(crate) async fn coordinated_threads(
        &self,
        session_id: &str,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<CoordinatedThreadLink>, SessionExecutionError> {
        let realizations = self.installed_sessions.realizations();
        if observed_root.is_some_and(|snapshot| snapshot.thread_id.0 != session_id) {
            return Err(SessionExecutionError::internal(
                "coordination query received a snapshot for a different Session root",
            ));
        }
        let delivered = realizations.published_snapshot(session_id)?;
        // Relationship recovery and terminal teardown must remain available
        // after a publication is revoked. The committed advisor call supplies
        // identity; current publication only enriches its model label. Managed
        // can recover an empty label from the Session's already-frozen profile.
        let advisor_model = self
            .realization_executor
            .environments()
            .publication()
            .resolve_session_publication(session_id, None, delivered)
            .ok()
            .and_then(|(_, _, publication, _)| publication)
            .and_then(|snapshot| {
                snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .advisor
                    .map(|advisor| advisor.model)
            });
        // One recovery reader and one relationship projector serve both an
        // ordinary latest read and a larger query that already holds this
        // atomic root prefix. Never reopen or clone that supplied prefix.
        let committed = match observed_root {
            Some(snapshot) => std::borrow::Cow::Borrowed(snapshot),
            None => {
                let Some(snapshot) = self
                    .session_thread_recovery_snapshot(session_id, &ThreadId(session_id.into()))
                    .await?
                else {
                    return Ok(Vec::new());
                };
                std::borrow::Cow::Owned(snapshot)
            }
        };
        let mut links = project_committed_coordination(
            &awaken_session_types::SessionId::from(session_id),
            advisor_model.as_deref(),
            &committed,
            CoordinationReadPurpose::Relationships,
        )
        .map(|projection| projection.links)?;
        if links.is_empty() {
            return Ok(links);
        }

        // Child Run facts live in their logical Threads and are intentionally
        // absent from the root snapshot. Enrich only from lifecycle facts at or
        // before the snapshot's backend watermark, never from a later prefix.
        let commit = self.commit_for_read(session_id).await?;
        let mut cursor = RunLifecycleCursor::default();
        loop {
            let page = commit
                .events_after(cursor, MAX_LIFECYCLE_PAGE)
                .await
                .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
            let reached_snapshot_fence =
                apply_lifecycle_latest_runs(&mut links, &page.events, committed.store_cursor);
            if reached_snapshot_fence || page.events.is_empty() || page.next_cursor == cursor {
                break;
            }
            cursor = page.next_cursor;
        }
        Ok(links)
    }

    /// Read the complete logical child prefix from the parent Session's sole
    /// physical commit authority. The existing recovery port owns consistency;
    /// this method only selects its claimed Run from committed child truth.
    pub(crate) async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecoverySnapshot>, SessionExecutionError> {
        let commit = self.commit_for_read(session_id).await?;
        let Some(latest_run) = commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(SessionExecutionError::internal)?
        else {
            return Ok(None);
        };
        commit
            .recovery_snapshot(thread_id, &latest_run.id)
            .await
            .map(Some)
            .map_err(|error| {
                SessionExecutionError::internal(format!(
                    "recover Session Thread {:?} from parent partition: {error}",
                    thread_id.0
                ))
            })
    }

    /// Read the exact claim-selected logical Run through its parent Session's
    /// physical commit partition. Registered-Worker recovery already holds the
    /// guarded [`RunDispatch`](awaken_dispatch_contract::RunDispatch), so it must not
    /// reopen a logical child as a second physical Session.
    pub(crate) async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, SessionExecutionError> {
        self.commit_for_read(session_id)
            .await?
            .recovery_snapshot(thread_id, run_id)
            .await
            .map_err(|error| {
                SessionExecutionError::internal(format!(
                    "recover Session Thread {:?} Run {:?} from parent partition: {error}",
                    thread_id.0, run_id.0
                ))
            })
    }

    pub(crate) async fn session_thread_usage(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<awaken_session_budget_contract::SessionUsage, SessionExecutionError> {
        let commit = self.commit_for_read(session_id).await?;
        if commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(SessionExecutionError::internal)?
            .is_none()
        {
            return Ok(awaken_session_budget_contract::SessionUsage::default());
        }
        // Usage depends only on committed State. Reuse its existing authority
        // query instead of reading every Message, Event, Run and resume ticket
        // again. The parent partition and absent-Run rule stay unchanged; shared
        // stores still bypass their process-local compatibility projections.
        let state = commit
            .committed_state_view(thread_id)
            .await
            .map_err(SessionExecutionError::internal)?;
        Ok(
            awaken_session_application_contract::session_usage_from_thread_usage(
                awaken_runtime_contract::llm::ThreadUsage::from_committed_state(&state),
            ),
        )
    }

    pub(crate) async fn session_thread_disposition(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<awaken_agent_contract::ThreadDisposition, SessionExecutionError> {
        let commit = self.commit_for_read(session_id).await?;
        let state = commit
            .committed_state_view(thread_id)
            .await
            .map_err(SessionExecutionError::internal)?;
        awaken_agent_contract::thread_disposition_from_committed_state(&state).map_err(|error| {
            SessionExecutionError::internal(format!(
                "recover disposition for Session Thread {:?}: {error}",
                thread_id.0
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::RunState;
    use awaken_agent_contract::agent::state::Scope;
    use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy};
    use awaken_agent_contract::thread::commit::RunDisposition;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;

    #[tokio::test]
    async fn coordination_query_reuses_only_the_supplied_root_prefix() {
        // Causes: C1 latest/supplied root; C2 absent/present committed Run;
        // C3 same/foreign root identity; C4 later valid/malformed transcript.
        // Rules Q1=latest+absent => empty and one authoritative selection;
        // Q2=supplied+same => exact prefix, zero additional root selections;
        // Q3=Q2+later valid commit => old links; latest => both links;
        // Q4=Q2+later malformed commit => old links; latest => source error;
        // Q5=supplied+foreign => reject before any source selection.
        // This uses the existing real in-memory commit/recovery adapter and
        // query-policy counter. Adjacent prefix tests own child lifecycle
        // watermark enrichment; Managed regressions own the final fresh fence.
        let session = "borrowed-coordination-root";
        let run = RunId("borrowed-coordination-run".into());
        let store = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
            awaken_time::system_clock(),
        ));
        let queries = crate::authority::ScriptedStateQueries::new(Ok(Vec::new()));
        let authority = Arc::new(
            crate::EphemeralRuntimeAuthority::new(awaken_time::system_clock()).with_commit(
                session,
                Arc::new(crate::LocalCommitAdapter::with_queries(
                    store.clone(),
                    queries.clone(),
                )),
            ),
        );
        let host = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::host::MemoryHostModel),
            "stub",
        )
        .await
        .with_runtime_authority(authority)
        .expect("local authority is selected before Host construction"))
        .build()
        .expect("validated Host dependencies");
        let managed = crate::host::session_executor(&host);
        assert!(
            managed
                .coordination_reads()
                .coordinated_threads(&awaken_session_types::SessionId::from(session), None)
                .await
                .unwrap()
                .is_empty(),
            "Q1"
        );
        assert_eq!(queries.latest_run_reads(), 1, "Q1 latest selection");
        let advisor = |step| {
            Message::new(
                MessageId::assistant(&run, step),
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    format!("advisor-{step}"),
                    awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                    serde_json::json!({}),
                )],
            )
        };
        store
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                ThreadId(session.into()),
                RunDisposition::running(run.clone()),
                true,
                vec![advisor(0)],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        let snapshot = host
            .thread_reads
            .session_thread_recovery_snapshot(session, &ThreadId(session.into()))
            .await
            .unwrap()
            .unwrap();
        let before = queries.latest_run_reads();
        let original = managed
            .coordination_reads()
            .coordinated_threads(
                &awaken_session_types::SessionId::from(session),
                Some(&snapshot),
            )
            .await
            .unwrap();
        assert_eq!(original.len(), 1, "Q2 exact advisor link");
        assert_eq!(
            queries.latest_run_reads(),
            before,
            "Q2 borrowed prefix skips root selection"
        );
        store
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                ThreadId(session.into()),
                RunDisposition::running(run.clone()),
                true,
                vec![advisor(1)],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        assert_eq!(
            managed
                .coordination_reads()
                .coordinated_threads(
                    &awaken_session_types::SessionId::from(session),
                    Some(&snapshot)
                )
                .await
                .unwrap(),
            original,
            "Q3 old prefix"
        );
        assert_eq!(
            queries.latest_run_reads(),
            before,
            "Q3 no hidden recovery query"
        );
        assert_eq!(
            managed
                .coordination_reads()
                .coordinated_threads(&awaken_session_types::SessionId::from(session), None)
                .await
                .unwrap()
                .len(),
            2,
            "Q3 fresh links"
        );
        assert_eq!(
            queries.latest_run_reads(),
            before + 1,
            "Q3 fresh selector remains active"
        );
        store
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                ThreadId(session.into()),
                RunDisposition::running(run.clone()),
                true,
                vec![Message::new(
                    MessageId::assistant(&run, 2),
                    Role::Assistant,
                    vec![ContentBlock::tool_use(
                        "malformed",
                        awaken_ext_builtin_tools::SEND_MESSAGE,
                        serde_json::json!(42),
                    )],
                )],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        let before = queries.latest_run_reads();
        assert_eq!(
            managed
                .coordination_reads()
                .coordinated_threads(
                    &awaken_session_types::SessionId::from(session),
                    Some(&snapshot)
                )
                .await
                .unwrap(),
            original,
            "Q4 unchanged observed truth"
        );
        assert_eq!(
            queries.latest_run_reads(),
            before,
            "Q4 no hidden recovery query"
        );
        let error = managed
            .coordination_reads()
            .coordinated_threads(&awaken_session_types::SessionId::from(session), None)
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .contains("committed send_message arguments are invalid"),
            "Q4 {error:?}"
        );
        let before = queries.latest_run_reads();
        let mut foreign = snapshot.clone();
        foreign.thread_id = ThreadId("another-session".into());
        let error = managed
            .coordination_reads()
            .coordinated_threads(
                &awaken_session_types::SessionId::from(session),
                Some(&foreign),
            )
            .await
            .unwrap_err();
        assert!(
            error.message.contains("different Session root"),
            "Q5 {error:?}"
        );
        assert_eq!(
            queries.latest_run_reads(),
            before,
            "Q5 rejected before source I/O"
        );
        assert_eq!(
            snapshot.messages.len(),
            1,
            "Q2-Q5 borrowed source never mutated"
        );
    }

    #[tokio::test]
    async fn thread_usage_reads_authoritative_state_without_rebuilding_history() {
        // Cause/effect graph: C1 root/child identity selects the same parent
        // partition; C2 an authoritative Run is absent/present; C3 State read
        // succeeds/fails; C4 the compatibility history stays stale; C5 a later
        // committed usage cell changes after a Host restart.
        // Rules U1=C1+!C2+!C3 => zero without reading State;
        // U2=C1+C2+!C3 => error, never fabricated zero;
        // U3=C1+C2+C3+C4 => exact authoritative totals and model attribution;
        // U4=U3+C5 => fresh totals without hydrating a transcript or child store.
        // The adjacent recovery test owns consistent multi-region snapshots;
        // usage needs only the existing authoritative State read and projector.
        for child in [false, true] {
            let session_id = "usage-state-parent";
            let thread_id = ThreadId(
                if child {
                    "usage-state-child"
                } else {
                    session_id
                }
                .into(),
            );
            let store = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            ));
            let queries =
                crate::authority::ScriptedStateQueries::new(Err("usage State unavailable".into()));
            let authority = Arc::new(
                crate::EphemeralRuntimeAuthority::new(awaken_time::system_clock()).with_commit(
                    session_id,
                    Arc::new(crate::LocalCommitAdapter::with_queries(
                        store.clone(),
                        queries.clone(),
                    )),
                ),
            );
            let make_host = || async {
                (crate::SessionExecutorBuilder::ephemeral(
                    Arc::new(crate::host::MemoryHostModel),
                    "stub",
                )
                .await
                .with_runtime_authority(authority.clone())
                .expect("local authority is selected before Host construction"))
                .build()
                .expect("validated Host dependencies")
            };
            let host = make_host().await;
            let managed = crate::host::session_executor(&host);
            assert_eq!(
                managed
                    .budget()
                    .session_thread_usage(
                        &awaken_session_types::SessionId::from(session_id),
                        &thread_id.0
                    )
                    .await
                    .unwrap(),
                awaken_session_budget_contract::SessionUsage::default(),
                "U1 absent Run does not read unavailable State"
            );
            store
                .commit(awaken_agent_contract::ThreadCommit::assemble(
                    thread_id.clone(),
                    RunDisposition::ended(
                        RunId("usage-state-run".into()),
                        awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                    ),
                    true,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ))
                .await
                .unwrap();
            let error = managed
                .budget()
                .session_thread_usage(
                    &awaken_session_types::SessionId::from(session_id),
                    &thread_id.0,
                )
                .await
                .expect_err("U2 unavailable State must not become zero usage");
            assert!(
                error.to_string().contains("usage State unavailable"),
                "U2 exact source error"
            );
            for tokens in [17, 29] {
                let mut usage = awaken_runtime_contract::llm::ThreadUsage::default();
                usage.record(
                    "usage-state-model",
                    awaken_runtime_contract::llm::TokenUsage {
                        prompt_tokens: tokens,
                        completion_tokens: 5,
                        ..Default::default()
                    },
                );
                queries.replace(Ok(vec![StateCommand::set(
                    Scope::Thread,
                    MergePolicy::Commutative,
                    awaken_runtime_contract::llm::THREAD_USAGE_STATE_KEY,
                    serde_json::to_value(usage).unwrap(),
                )]));
                let restarted = crate::host::session_executor(&make_host().await);
                let actual = restarted
                    .budget()
                    .session_thread_usage(
                        &awaken_session_types::SessionId::from(session_id),
                        &thread_id.0,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    actual.input_tokens, tokens,
                    "U3/U4 current authoritative totals"
                );
                assert_eq!(actual.output_tokens, 5, "U3/U4 output totals");
                assert_eq!(
                    actual.by_model["usage-state-model"].input_tokens, tokens,
                    "U3/U4 model attribution"
                );
                assert!(awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::committed_state(store.as_ref(), &thread_id).is_empty(),
                    "U3/U4 compatibility State remains stale; no history hydration");
            }
            if child {
                assert!(
                    !crate::RuntimeAuthority::durable_thread_exists(
                        authority.as_ref(),
                        &thread_id.0
                    )
                    .await
                    .unwrap(),
                    "U4 child usage never opens a child-named partition"
                );
            }
        }
    }

    #[tokio::test]
    async fn child_recovery_snapshot_reuses_the_parent_commit_consistency_boundary() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 the logical child has/has-not a committed Run;
        // C2 child and unrelated logical commits share the parent physical
        // partition; C3 the latest child Run is Awaiting with transcript, state,
        // cumulative usage, and a resume ticket. Effects: E1 no Run returns None without creating
        // child storage and projects zero usage; E2 one recovery read returns a mutually consistent child
        // prefix; E3 the snapshot retains the backend-wide cursor while excluding
        // unrelated logical facts; E4 ManagedHost delegates to this same read;
        // E5 the registered-Worker recovery adapter preserves the guarded parent
        // partition while retaining the child's logical identity; E6 usage is
        // projected from the same committed State authority, never a process-local
        // compatibility cache; C4 the caller supplies an exact present/absent
        // Run id. E7 exact recovery reuses the same prefix for the present Run
        // and fails closed for the absent Run.
        // C5 the pure projection selector is None/exact older/absent while a
        // later Run commits in the same logical child. E8 None preserves latest,
        // exact preserves the older claimed Run AND all historical facts, absent
        // stays None; E9 none of these reads prepares or installs an Environment.
        // R3=C5(None|older|absent) => E8,E9 with the same parent partition.
        //
        // | Rule | C1 | C2 | C3 | Effects       |
        // | R1   | N  | -  | -  | E1            |
        // | R2   | Y  | Y  | Y  | E2,E3,E4,E5,E6,E7 |
        let session_id = "snapshot-parent-session";
        let child_id = ThreadId("snapshot-logical-child".into());
        let child_run = RunId("snapshot-child-run".into());
        let host = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::host::MemoryHostModel),
            "stub",
        )
        .await)
            .build()
            .expect("validated Host dependencies");

        assert_eq!(
            host.thread_reads
                .session_thread_recovery_snapshot(session_id, &child_id)
                .await
                .expect("R1 parent-partition read"),
            None,
            "R1/E1"
        );
        assert!(
            crate::host::session_executor(&host)
                .projection()
                .projection_thread_snapshot(
                    &awaken_session_types::SessionId::from(session_id),
                    &child_id.0,
                    None
                )
                .await
                .unwrap()
                .is_none(),
            "R1/E1 pure selector keeps missing history absent"
        );
        assert!(
            !host
                .run_assembly
                .authority
                .durable_thread_exists(&child_id.0)
                .await
                .expect("R1 child partition probe"),
            "R1/E1 does not open a child-named physical partition"
        );
        assert_eq!(
            crate::host::session_executor(&host)
                .budget()
                .session_thread_usage(
                    &awaken_session_types::SessionId::from(session_id),
                    &child_id.0
                )
                .await
                .expect("R1 absent usage read"),
            awaken_session_budget_contract::SessionUsage::default(),
            "R1/E1 only a genuinely absent Run projects zero usage",
        );

        let parent_commit = host
            .thread_reads
            .commit_for_read(session_id)
            .await
            .expect("parent physical commit authority");
        parent_commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                child_id.clone(),
                RunDisposition::running(child_run.clone()),
                true,
                vec![Message::text(
                    MessageId("snapshot-child-user".into()),
                    Role::User,
                    "child question",
                )],
                vec![StateCommand::set(
                    Scope::Thread,
                    MergePolicy::Disjoint,
                    "snapshot-phase",
                    serde_json::json!("running"),
                )],
                Vec::new(),
            ))
            .await
            .expect("R2 first child commit");
        parent_commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                ThreadId("snapshot-unrelated-logical-thread".into()),
                RunDisposition::running(RunId("snapshot-unrelated-run".into())),
                true,
                vec![Message::text(
                    MessageId("snapshot-unrelated-message".into()),
                    Role::User,
                    "must stay outside the child snapshot",
                )],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("R2 unrelated logical commit");
        let ticket = ResumeTicket::new(
            "snapshot-correlation",
            child_run.clone(),
            child_id.clone(),
            "snapshot-id",
            "snapshot-catalog",
            AwaitTarget::RemoteInput {
                reason: awaken_agent_contract::agent::awaiting::RemoteInputReason::UserInput,
                call_id: "snapshot-call".into(),
            },
        );
        parent_commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                child_id.clone(),
                RunDisposition::awaiting(ticket.clone()),
                true,
                vec![Message::text(
                    MessageId("snapshot-child-assistant".into()),
                    Role::Assistant,
                    "child needs input",
                )],
                vec![
                    StateCommand::set(
                        Scope::Thread,
                        MergePolicy::Disjoint,
                        "snapshot-phase",
                        serde_json::json!("awaiting"),
                    ),
                    StateCommand::set(
                        Scope::Thread,
                        MergePolicy::Commutative,
                        awaken_runtime_contract::llm::THREAD_USAGE_STATE_KEY,
                        serde_json::to_value(awaken_runtime_contract::llm::ThreadUsage {
                            by_model: std::collections::BTreeMap::from([(
                                "snapshot-model".into(),
                                awaken_runtime_contract::llm::TokenUsage {
                                    prompt_tokens: 13,
                                    completion_tokens: 5,
                                    cache_read_tokens: 3,
                                    cache_creation_tokens: 2,
                                },
                            )]),
                        })
                        .expect("serialize R2 usage"),
                    ),
                ],
                Vec::new(),
            ))
            .await
            .expect("R2 awaiting child commit");

        let managed = crate::host::session_executor(&host);
        let snapshot = managed
            .thread_reads()
            .session_thread_recovery_snapshot(
                &awaken_session_types::SessionId::from(session_id),
                &child_id.0,
            )
            .await
            .expect("R2 ManagedHost read")
            .expect("R2 committed child Run");
        assert_eq!(snapshot.thread_id, child_id, "R2/E2 logical Thread");
        assert_eq!(snapshot.claimed_run_id, child_run, "R2/E2 claimed Run");
        assert_eq!(
            snapshot.latest_run_id,
            Some(child_run.clone()),
            "R2/E2 latest"
        );
        assert_eq!(snapshot.runs.len(), 1, "R2/E2 one child Run");
        assert_eq!(snapshot.runs[0].state, RunState::Awaiting, "R2/E2 state");
        assert_eq!(
            snapshot
                .messages
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>(),
            ["child question", "child needs input"],
            "R2/E2 child transcript is one consistent prefix"
        );
        assert_eq!(snapshot.state.len(), 3, "R2/E2 all child state commands");
        assert_eq!(snapshot.resume_tickets.len(), 1, "R2/E2 active ticket");
        assert_eq!(
            snapshot.resume_tickets[0].run_id, child_run,
            "R2/E2 ticket owner"
        );
        assert_eq!(snapshot.resume_tickets[0].ticket, ticket, "R2/E2 ticket");
        assert_eq!(snapshot.thread_version, 2, "R2/E3 child-only version");
        assert_eq!(snapshot.store_cursor, 3, "R2/E3 parent-store cursor");
        assert_eq!(snapshot.next_commit_ordinal, 2, "R2/E2 claimed ordinal");
        let exact_snapshot = managed
            .thread_reads()
            .session_thread_run_recovery_snapshot(
                &awaken_session_types::SessionId::from(session_id),
                &child_id.0,
                &child_run,
            )
            .await
            .expect("R2 ManagedHost exact read")
            .expect("R2 exact committed child Run");
        let usage = managed
            .budget()
            .session_thread_usage(
                &awaken_session_types::SessionId::from(session_id),
                &child_id.0,
            )
            .await
            .expect("R2 ManagedHost usage read");
        assert_eq!(usage.input_tokens, 13, "R2/E6 authoritative usage");
        assert_eq!(usage.output_tokens, 5, "R2/E6 authoritative usage");
        assert_eq!(
            usage.by_model["snapshot-model"].cache_read_tokens, 3,
            "R2/E6 per-model attribution",
        );
        assert_eq!(exact_snapshot, snapshot, "R2/E7 exact recovery authority");
        assert_eq!(
            managed
                .thread_reads()
                .session_thread_run_recovery_snapshot(
                    &awaken_session_types::SessionId::from(session_id),
                    &child_id.0,
                    &RunId("snapshot-absent-run".into()),
                )
                .await
                .expect("R2 absent exact read"),
            None,
            "R2/E7 absent exact Run fails closed"
        );
        let worker_snapshot = host
            .worker_recovery_source()
            .recovery_snapshot_in_session(&ThreadId(session_id.into()), &child_id, &child_run)
            .await
            .expect("R2 registered-Worker parent-partition read");
        assert_eq!(worker_snapshot, snapshot, "R2/E5 one recovery authority");
        assert!(
            !host
                .run_assembly
                .authority
                .durable_thread_exists(&child_id.0)
                .await
                .expect("R2 child partition probe"),
            "R2/E4-E5 child truth remains physically in the parent partition"
        );

        parent_commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                child_id.clone(),
                RunDisposition::ended(
                    child_run.clone(),
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("R3 settle the original child before the next Run");
        let next_run = RunId("snapshot-next-child-run".into());
        parent_commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                child_id.clone(),
                RunDisposition::running(next_run.clone()),
                true,
                vec![Message::text(
                    MessageId("snapshot-next-user".into()),
                    Role::User,
                    "next child question",
                )],
                vec![StateCommand::set(
                    Scope::Run,
                    MergePolicy::Disjoint,
                    "next-run-state",
                    serde_json::json!(true),
                )],
                Vec::new(),
            ))
            .await
            .expect("R3 newer Run in the original parent partition");
        let parent = awaken_session_types::SessionId::from(session_id);
        let latest = managed
            .projection()
            .projection_thread_snapshot(&parent, &child_id.0, None)
            .await
            .unwrap()
            .unwrap();
        let older = managed
            .projection()
            .projection_thread_snapshot(&parent, &child_id.0, Some(&child_run))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            latest.claimed_run_id, next_run,
            "R3/E8 None keeps latest selection"
        );
        assert_eq!(
            older.claimed_run_id, child_run,
            "R3/E8 exact selection never falls back to latest"
        );
        assert_eq!(
            older.latest_run_id,
            Some(next_run),
            "R3/E8 latest remains a separate coordinate"
        );
        assert_eq!(older.runs.len(), 2, "R3/E8 both retained Runs");
        assert_eq!(
            older.messages, latest.messages,
            "R3/E8 all transcript history"
        );
        assert_eq!(older.state, latest.state, "R3/E8 all state history");
        assert_eq!(
            older.message_commit_cursors, latest.message_commit_cursors,
            "R3/E8 original message coordinates"
        );
        assert_eq!(
            older.state_commit_cursors, latest.state_commit_cursors,
            "R3/E8 original state coordinates"
        );
        assert_eq!(
            older.next_commit_ordinal, 3,
            "R3/E8 original selected Run ordinal"
        );
        assert_eq!(
            latest.next_commit_ordinal, 1,
            "R3/E8 newer selected Run ordinal"
        );
        assert!(
            managed
                .projection()
                .projection_thread_snapshot(
                    &parent,
                    &child_id.0,
                    Some(&RunId("snapshot-absent-run".into()))
                )
                .await
                .unwrap()
                .is_none(),
            "R3/E8 absent is not latest"
        );
        for thread in [session_id, child_id.0.as_str()] {
            assert!(
                host.realization_executor
                    .environments()
                    .session_environment(thread)
                    .await
                    .expect("installed Sessions")
                    .is_none(),
                "R3/E9 pure reads never prepare/install an Environment"
            );
        }
        assert!(
            !host
                .run_assembly
                .authority
                .durable_thread_exists(&child_id.0)
                .await
                .unwrap(),
            "R3/E8-E9 no child physical partition"
        );
    }
}
