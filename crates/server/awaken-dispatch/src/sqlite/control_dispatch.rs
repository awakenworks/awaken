// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: SQLite implementation of the existing ControlDispatchQueue, not a second
// @formal reason: claim or credential authority. Dynamic path: exact owner/epoch/live-lease predicates
// @formal reason: gate credential-binding reads; Session reservations and activation invoke canonical
// @formal reason: classifiers inside BEGIN IMMEDIATE before row/receipt publication. Shared reservation
// @formal reason: and credential conformance own executable evidence; SQL, serde and async refinement are
// @formal reason: not claimed proved.
// SQLite control-only Dispatch authority. Included beside DispatchQueue.
#[async_trait]
impl ControlDispatchQueue for SqliteDispatchStore {
    async fn finish_attempt_closure(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        self.finish_physical_attempt_with(claim, true).await
    }

    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &awaken_runtime_contract::CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError> {
        let claim = claim.clone();
        let candidate = candidate.clone();
        let now_ms = self.clock.now_ms();
        self.with_conn(move |conn, p| {
            let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
            let now_ms = durable_i64("dispatch store time", now_ms)?;
            let bindings_json: Option<Option<String>> = conn
                .query_row(
                    &format!(
                        "SELECT credential_bindings FROM {p}_dispatch WHERE run_id = ?1 \
                         AND status = 'running' AND lease_owner = ?2 AND lease_epoch = ?3 \
                         AND lease_until > ?4"
                    ),
                    params![claim.run_id.0, claim.owner, claim_epoch, now_ms],
                    |row| row.get(0),
                )
                .optional()
                .map_err(store_error)?;
            let bindings: Vec<AttemptCredentialBinding> = bindings_json
                .flatten()
                .map(|value| serde_json::from_str(&value).map_err(json_err))
                .transpose()?
                .unwrap_or_default();
            Ok(bindings
                .into_iter()
                .find(|binding| binding.candidate_fingerprint == candidate))
        })
        .await
    }

    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        let (request, reservation_ttl_ms) =
            validate_session_run_reservation_request(request, reservation_ttl_ms)?;
        let deadline = crate::clock::deadline_millis(self.clock.now_ms(), reservation_ttl_ms);
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let live: Option<(String, String)> = tx
                .query_row(
                    &format!("SELECT status, request FROM {p}_dispatch WHERE run_id = ?1"),
                    params![request.run_id().0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(store_error)?;
            if let Some((status, stored_json)) = live {
                let stored: RunDispatch = serde_json::from_str(&stored_json).map_err(json_err)?;
                let state = DispatchState::from_db(&status).ok_or_else(|| {
                    DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
                })?;
                let outcome = classify_live_session_run_reservation(&stored, state, &request);
                tx.commit().map_err(store_error)?;
                return Ok(outcome);
            }
            let completed: Option<Option<String>> = tx
                .query_row(
                    &format!(
                        "SELECT request_fingerprint FROM {p}_dispatch_completion WHERE run_id = ?1"
                    ),
                    params![request.run_id().0],
                    |row| row.get(0),
                )
                .optional()
                .map_err(store_error)?;
            if let Some(request_fingerprint) = completed {
                let outcome = classify_completed_session_run_reservation(
                    request_fingerprint.as_deref(),
                    &request,
                );
                tx.commit().map_err(store_error)?;
                return Ok(outcome);
            } else {
                let options = SubmitOptions {
                    supersede: request.session_run_replacement.supersedes_prior(),
                    ..Default::default()
                };
                insert_dispatch_with_state(
                    &tx,
                    p,
                    &request,
                    &options,
                    "reserved",
                    Some(crate::clock::db_millis(deadline)),
                )?;
            }
            tx.commit().map_err(store_error)?;
            Ok(SessionRunReservationOutcome::Reserved)
        })
        .await
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        let run_id = run_id.clone();
        let session_thread_id = session_thread_id.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let current: Option<(String, String, i64, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT status, request, lease_epoch, cancel_requested \
                         FROM {p}_dispatch WHERE run_id = ?1"
                    ),
                    params![run_id.0],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()
                .map_err(store_error)?;
            let Some((status, request_json, lease_epoch, cancellation_requested)) = current else {
                let completed = tx
                    .query_row(
                        &format!("SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1"),
                        params![run_id.0],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(store_error)?
                    .is_some();
                let outcome = classify_session_run_reservation_activation(
                    None,
                    completed,
                    &session_thread_id,
                    session_activity_epoch,
                )?
                .expect("missing reservation always has a closed outcome");
                tx.commit().map_err(store_error)?;
                return Ok(outcome);
            };
            let mut request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
            let state = DispatchState::from_db(&status).ok_or_else(|| {
                DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
            })?;
            let classified = classify_session_run_reservation_activation(
                Some((&request, state)),
                false,
                &session_thread_id,
                session_activity_epoch,
            )?;
            let outcome = if let Some(outcome) = classified {
                outcome
            } else {
                let next = crate::persisted_dispatch_transition(
                    &status,
                    lease_epoch,
                    cancellation_requested != 0,
                )?
                .activate_reservation()
                .expect("the reservation classifier admitted only Reserved");
                request.session_activity_epoch = Some(session_activity_epoch);
                let changed = tx
                    .execute(
                        &format!(
                            "UPDATE {p}_dispatch SET request = ?1, status = ?3, \
                             lease_owner = NULL, lease_until = NULL WHERE run_id = ?2 \
                             AND status = 'reserved'"
                        ),
                        params![
                            json(&request)?,
                            run_id.0,
                            crate::dispatch_state_db(next.state)
                        ],
                    )
                    .map_err(store_error)?;
                if changed == 1 {
                    SessionRunReservationActivation::Activated
                } else {
                    SessionRunReservationActivation::RecoveryClaimed
                }
            };
            tx.commit().map_err(store_error)?;
            Ok(outcome)
        })
        .await
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let run_id = run_id.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let current: Option<(String, i64, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT status, lease_epoch, cancel_requested FROM {p}_dispatch \
                         WHERE run_id = ?1"
                    ),
                    params![run_id.0],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(store_error)?;
            let removable = if let Some((status, lease_epoch, cancellation_requested)) = current {
                matches!(
                    crate::persisted_dispatch_transition(
                        &status,
                        lease_epoch,
                        cancellation_requested != 0,
                    )?
                    .reject_reservation(),
                    awaken_dispatch_contract::GuardedTransition::Removed
                )
            } else {
                false
            };
            if !removable {
                tx.commit().map_err(store_error)?;
                return Ok(false);
            }
            let changed = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE run_id = ?1 AND status = 'reserved'"),
                    params![run_id.0],
                )
                .map_err(store_error)?;
            if changed == 1 {
                tx.execute(
                    &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                    params![run_id.0],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(changed == 1)
        })
        .await
    }

    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            if exact_run_replay(&tx, p, &request)? {
                tx.commit().map_err(store_error)?;
                return Ok(());
            }
            let parent = session_child_parent(&request)?.clone();
            ensure_session_child_capacity(
                &request,
                &admission,
                known_session_child_threads(&tx, p, &parent)?,
            )?;
            insert_new_dispatch(&tx, p, &request, &SubmitOptions::default())?;
            tx.commit().map_err(store_error)?;
            Ok(())
        })
        .await
    }

    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "reservation_running")
            .await
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let requester = requester.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let thread_available = thread_available_for_claim(p);
            let sql = format!(
                "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
                 ((d.status IN ('reserved', 'reservation_running') AND \
                   d.lease_until IS NOT NULL AND d.lease_until < ?1 AND {}) OR \
                 (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?1) OR \
                 (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
                   WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= ?1)) \
                   ) AND {thread_available}) OR \
                 (d.status = 'pending' AND {thread_available})) \
                 AND (d.retry_not_before IS NULL OR d.retry_not_before <= ?1) \
                 ORDER BY CASE WHEN d.active_attempt_owner IS NOT NULL OR d.active_attempt_epoch IS NOT NULL THEN 1 ELSE 0 END, \
                          CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                          d.priority DESC, d.created_at",
                no_running_peer(p)
            );
            let capabilities = installed_worker_credential_capabilities(&requester)?;
            let selected = {
                let mut stmt = tx.prepare(&sql).map_err(store_error)?;
                let rows = stmt
                    .query_map(params![crate::clock::db_millis(now_ms)], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)? != 0,
                            row.get::<_, i64>(6)?,
                        ))
                    })
                    .map_err(store_error)?;
                let mut selected = None;
                for row in rows {
                    let (
                        run_id,
                        request_json,
                        sandbox,
                        previous_json,
                        status,
                        cancellation_requested,
                        lease_epoch,
                    ) = row.map_err(store_error)?;
                    let request: RunDispatch =
                        serde_json::from_str(&request_json).map_err(json_err)?;
                    let previous: Option<WorkerAssignment> = previous_json
                        .map(|value| serde_json::from_str(&value).map_err(json_err))
                        .transpose()?;
                    let next_epoch = next_claim_epoch(lease_epoch)?;
                    let credentials_admitted = can_admit_claim_credentials(
                        &request,
                        &capabilities,
                        next_epoch,
                        now_ms,
                        claim_credential_operation(false, cancellation_requested),
                    );
                    if matches!(status.as_str(), "reserved" | "reservation_running")
                        || (cancellation_requested && credentials_admitted)
                        || (policy_selects_requester(
                            &request,
                            policy.as_ref(),
                            DispatchPlacement {
                                recovered: status == "running",
                                previous: previous.as_ref(),
                                sandbox_bound: sandbox.is_some(),
                                requester: &requester.identity,
                                workers: &workers,
                                now_ms,
                            },
                        )? && credentials_admitted)
                    {
                        selected = Some(run_id);
                        break;
                    }
                }
                selected
            };
            let Some(run_id) = selected else {
                return Ok(None);
            };
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &requester.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&requester),
                &capabilities,
            )?;
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_for_terminal_recovery(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let requested_run = requested_run.0.clone();
        let owner = owner.to_string();
        self.with_conn(move |conn, _p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let claimed = claim_exact_transaction_with_mode(
                &tx,
                &requested_run,
                &owner,
                lease_ms,
                now_ms,
                None,
                &Default::default(),
                ExactClaimMode::TerminalRecovery,
            )?;
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn runnable_depth(&self) -> Result<Option<u64>, DispatchError> {
        self.query_runnable_depth().await
    }

    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<PhysicalAttempt>, DispatchError> {
        let run_id = run_id.clone();
        self.with_conn(move |conn, p| {
            let row: Option<(Option<String>, Option<i64>, Option<String>, String, bool)> = conn.query_row(
                &format!("SELECT active_attempt_owner, active_attempt_epoch, active_attempt_binding, request, active_attempt_closing FROM {p}_dispatch WHERE run_id = ?1"),
                params![run_id.0], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            ).optional().map_err(store_error)?;
            let Some((owner, epoch, binding, request_json, closing)) = row else { return Ok(None); };
            let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
            crate::dispatch::decode_physical_attempt(
                &run_id, owner, epoch.map(|v| durable_u64("dispatch attempt epoch", v)).transpose()?, binding,
            ).map(|attempt| attempt.map(|mut attempt| {
                attempt.closing = closing;
                attempt.thread_id = Some(request.thread_id().clone());
                attempt.session_thread_id = request.session_thread_id.clone();
                attempt
            }))
        }).await
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let after_sequence = i64::try_from(after_sequence).map_err(|_| {
            DispatchError::Rejected("completion cursor exceeds INTEGER range".to_string())
        })?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn(move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT sequence, run_id, request_fingerprint, thread_id, session_thread_id \
                     FROM {p}_dispatch_completion \
                     WHERE sequence > ?1 ORDER BY sequence LIMIT ?2"
                ))
                .map_err(store_error)?;
            let rows = statement
                .query_map(params![after_sequence, limit], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })
                .map_err(store_error)?;
            let mut events = Vec::new();
            for row in rows {
                let (sequence, run_id, request_fingerprint, thread_id, session_thread_id) =
                    row.map_err(store_error)?;
                events.push(DispatchCompletion {
                    sequence: u64::try_from(sequence).map_err(|_| {
                        DispatchError::Rejected(
                            "persisted completion sequence is negative".to_string(),
                        )
                    })?,
                    run_id: RunId(run_id),
                    thread_id: thread_id.map(ThreadId),
                    session_thread_id: session_thread_id.map(ThreadId),
                    request_fingerprint,
                });
            }
            Ok(events)
        })
        .await
    }

    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, DispatchError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).map_err(|_| {
            DispatchError::Rejected("dispatch scan limit exceeds database range".into())
        })?;
        let after = after.map(|id| id.0.clone());
        self.with_conn(move |conn, p| {
            let clause = if after.is_some() {
                "WHERE run_id > ?2"
            } else {
                ""
            };
            let mut statement = conn
                .prepare(&format!(
                    "SELECT run_id FROM {p}_dispatch {clause} ORDER BY run_id LIMIT ?1"
                ))
                .map_err(store_error)?;
            let mut rows = if let Some(after) = after {
                statement.query(params![limit, after])
            } else {
                statement.query(params![limit])
            }
            .map_err(store_error)?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next().map_err(store_error)? {
                ids.push(RunId(row.get(0).map_err(store_error)?));
            }
            Ok(ids)
        })
        .await
    }

    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<DispatchSummary>, DispatchError> {
        Ok(self
            .read_dispatch_summaries(Some(run_id.0.clone()))
            .await?
            .pop())
    }
}
