// SQLite realization of the one neutral DispatchQueue contract. Included by
// `sqlite.rs` so the public adapter and trait implementation paths do not move.
#[async_trait]
impl DispatchQueue for SqliteDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
        now_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let run = run_id.0.clone();
        let owner = identity.lease_owner();
        self.with_conn(move |conn, p| {
            let epoch = conn
                .query_row(
                    &format!(
                        "SELECT lease_epoch FROM {p}_dispatch \
                     WHERE run_id = ?1 AND status = 'running' \
                     AND lease_owner = ?2 AND lease_until IS NOT NULL \
                     AND lease_until >= ?3 AND cancel_requested = 0"
                    ),
                    params![&run, &owner, crate::clock::db_millis(now_ms)],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(reject)?;
            epoch
                .map(|epoch| {
                    Ok(RunClaim {
                        run_id: RunId(run),
                        owner,
                        epoch: durable_u64("dispatch lease epoch", epoch)?,
                    })
                })
                .transpose()
        })
        .await
    }

    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        let reservation_ttl_ms =
            validate_session_run_reservation_request(&request, reservation_ttl_ms)?;
        let deadline = crate::clock::deadline_millis(self.clock.now_ms(), reservation_ttl_ms);
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let live: Option<(String, String)> = tx
                .query_row(
                    &format!("SELECT status, request FROM {p}_dispatch WHERE run_id = ?1"),
                    params![request.run_id().0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            if let Some((status, stored_json)) = live {
                let stored: RunDispatch = serde_json::from_str(&stored_json).map_err(json_err)?;
                let state = DispatchState::from_db(&status).ok_or_else(|| {
                    DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
                })?;
                let outcome = classify_live_session_run_reservation(&stored, state, &request);
                tx.commit().map_err(reject)?;
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
                .map_err(reject)?;
            if let Some(request_fingerprint) = completed {
                let outcome = classify_completed_session_run_reservation(
                    request_fingerprint.as_deref(),
                    &request,
                );
                tx.commit().map_err(reject)?;
                return Ok(outcome);
            } else {
                insert_dispatch_with_state(
                    &tx,
                    p,
                    &request,
                    &SubmitOptions::default(),
                    "reserved",
                    Some(crate::clock::db_millis(deadline)),
                )?;
            }
            tx.commit().map_err(reject)?;
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
                .map_err(reject)?;
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
                .map_err(reject)?;
            let Some((status, request_json, lease_epoch, cancellation_requested)) = current else {
                let completed = tx
                    .query_row(
                        &format!("SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1"),
                        params![run_id.0],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(reject)?
                    .is_some();
                let outcome = classify_session_run_reservation_activation(
                    None,
                    completed,
                    &session_thread_id,
                    session_activity_epoch,
                )?
                .expect("missing reservation always has a closed outcome");
                tx.commit().map_err(reject)?;
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
                    .map_err(reject)?;
                if changed == 1 {
                    SessionRunReservationActivation::Activated
                } else {
                    SessionRunReservationActivation::RecoveryClaimed
                }
            };
            tx.commit().map_err(reject)?;
            Ok(outcome)
        })
        .await
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let run_id = run_id.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
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
                .map_err(reject)?;
            let removable = if let Some((status, lease_epoch, cancellation_requested)) = current {
                matches!(
                    crate::persisted_dispatch_transition(
                        &status,
                        lease_epoch,
                        cancellation_requested != 0,
                    )?
                    .reject_reservation(),
                    awaken_run_ingress_contract::GuardedTransition::Removed
                )
            } else {
                false
            };
            if !removable {
                tx.commit().map_err(reject)?;
                return Ok(false);
            }
            let changed = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE run_id = ?1 AND status = 'reserved'"),
                    params![run_id.0],
                )
                .map_err(reject)?;
            if changed == 1 {
                tx.execute(
                    &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                    params![run_id.0],
                )
                .map_err(reject)?;
            }
            tx.commit().map_err(reject)?;
            Ok(changed == 1)
        })
        .await
    }

    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        let resolution = validate_session_run_reservation_resolution(resolution)?;
        let store_now_ms = self.clock.now_ms();
        let claim = claim.clone();
        self.with_conn(move |conn, p| {
            let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let current: Option<(String, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT request, cancel_requested FROM {p}_dispatch WHERE run_id = ?1 \
                         AND status = 'reservation_running' AND lease_owner = ?2 \
                         AND lease_epoch = ?3"
                    ),
                    params![claim.run_id.0, claim.owner, claim_epoch],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            let Some((request_json, cancellation_requested)) = current else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let mut request: RunDispatch =
                serde_json::from_str(&request_json).map_err(json_err)?;
            let transition = crate::persisted_dispatch_transition(
                "reservation_running",
                claim_epoch,
                cancellation_requested != 0,
            )?
            .resolve_reservation(
                claim.epoch,
                true,
                match resolution {
                    SessionRunReservationResolution::Admitted { .. } => Some(true),
                    SessionRunReservationResolution::Retry { .. } => Some(false),
                    SessionRunReservationResolution::Rejected => None,
                },
            );
            let changed = match resolution {
                SessionRunReservationResolution::Admitted {
                    session_activity_epoch,
                } => {
                    request.session_activity_epoch = Some(session_activity_epoch);
                    let awaken_run_ingress_contract::GuardedTransition::Applied(next) = transition
                    else {
                        unreachable!("the exact recovery claim is admitted by the kernel")
                    };
                    tx.execute(
                        &format!(
                            "UPDATE {p}_dispatch SET request = ?1, status = ?5, \
                             lease_owner = NULL, lease_until = NULL, worker_assignment = NULL, \
                             credential_bindings = NULL, credential_receipts = NULL \
                             WHERE run_id = ?2 AND status = 'reservation_running' \
                             AND lease_owner = ?3 AND lease_epoch = ?4"
                        ),
                        params![
                            json(&request)?,
                            claim.run_id.0,
                            claim.owner,
                            claim_epoch,
                            crate::dispatch_state_db(next.state)
                        ],
                    )
                    .map_err(reject)?
                }
                SessionRunReservationResolution::Retry { reservation_ttl_ms } => {
                    let awaken_run_ingress_contract::GuardedTransition::Applied(next) = transition
                    else {
                        unreachable!("the exact recovery claim is admitted by the kernel")
                    };
                    tx.execute(
                        &format!(
                            "UPDATE {p}_dispatch SET status = ?5, lease_owner = NULL, \
                             lease_until = ?1, worker_assignment = NULL, credential_bindings = NULL, \
                             credential_receipts = NULL, \
                             created_at = strftime('%Y-%m-%d %H:%M:%f', 'now') \
                             WHERE run_id = ?2 AND status = 'reservation_running' \
                             AND lease_owner = ?3 AND lease_epoch = ?4"
                        ),
                        params![
                            crate::clock::db_millis(crate::clock::deadline_millis(
                                store_now_ms,
                                reservation_ttl_ms,
                            )),
                            claim.run_id.0,
                            claim.owner,
                            claim_epoch,
                            crate::dispatch_state_db(next.state)
                        ],
                    )
                    .map_err(reject)?
                }
                SessionRunReservationResolution::Rejected => {
                    assert!(matches!(
                        transition,
                        awaken_run_ingress_contract::GuardedTransition::Removed
                    ));
                    let changed = tx
                        .execute(
                            &format!(
                                "DELETE FROM {p}_dispatch WHERE run_id = ?1 \
                                 AND status = 'reservation_running' AND lease_owner = ?2 \
                                 AND lease_epoch = ?3"
                            ),
                            params![claim.run_id.0, claim.owner, claim_epoch],
                        )
                        .map_err(reject)?;
                    if changed == 1 {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                            params![claim.run_id.0],
                        )
                        .map_err(reject)?;
                    }
                    changed
                }
            };
            if changed != 1 {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            tx.commit().map_err(reject)?;
            Ok(SettleOutcome::Applied)
        })
        .await
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;

            // A replayed completed run is a no-op before supersession, so it
            // cannot mutate sibling rows on its thread (ADR-0060).
            if exact_run_replay(&tx, p, &request)? {
                tx.commit().map_err(reject)?;
                return Ok(());
            }

            validate_executable_dispatch_admission(&request)?;
            insert_new_dispatch(&tx, p, &request, &options)?;
            tx.commit().map_err(reject)?;
            Ok(())
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
                .map_err(reject)?;
            if exact_run_replay(&tx, p, &request)? {
                tx.commit().map_err(reject)?;
                return Ok(());
            }
            let parent = session_child_parent(&request)?.clone();
            ensure_session_child_capacity(
                &request,
                &admission,
                known_session_child_threads(&tx, p, &parent)?,
            )?;
            insert_new_dispatch(&tx, p, &request, &SubmitOptions::default())?;
            tx.commit().map_err(reject)?;
            Ok(())
        })
        .await
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let run_id = request.run_id().0.clone();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            if exact_run_replay(&tx, p, &request)? {
                let claimed = claim_exact_transaction(
                    &tx,
                    &run_id,
                    &owner,
                    lease_ms,
                    now_ms,
                    None,
                    &capabilities,
                )?;
                tx.commit().map_err(reject)?;
                return Ok(claimed);
            }
            validate_executable_dispatch_admission(&request)?;
            if !can_claim_locally(&request.placement) {
                tx.commit().map_err(reject)?;
                return Ok(None);
            }
            insert_new_dispatch(&tx, p, &request, &SubmitOptions::default())?;
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                None,
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let run_id = request.run_id().0.clone();
        let worker = worker.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let owner = worker.identity.lease_owner();
            if exact_run_replay(&tx, p, &request)? {
                let claimed = claim_exact_transaction(
                    &tx,
                    &run_id,
                    &owner,
                    lease_ms,
                    now_ms,
                    Some(&worker),
                    &installed_worker_credential_capabilities(&worker)?,
                )?;
                tx.commit().map_err(reject)?;
                return Ok(claimed);
            }
            validate_executable_dispatch_admission(&request)?;
            if can_assign(&worker, &request.placement, None, false, now_ms).is_err() {
                tx.commit().map_err(reject)?;
                return Ok(None);
            }
            insert_new_dispatch(&tx, p, &request, &SubmitOptions::default())?;
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                Some(&worker),
                &installed_worker_credential_capabilities(&worker)?,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.0.clone();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            append_pending_row(&tx, p, &input)?;
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                None,
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.0.clone();
        let worker = worker.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            append_pending_row(&tx, p, &input)?;
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
                &installed_worker_credential_capabilities(&worker)?,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "running").await
    }

    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "reservation_running")
            .await
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;

            // Priority: recover an expired lease, then wake an awaiting run with
            // pending input, then a fresh pending run. SQLite has no SKIP LOCKED;
            // the IMMEDIATE transaction is the single-owner guard.
            let local_eligible = "(d.cancel_requested = 1 OR (\
                COALESCE(json_extract(d.request, '$.placement.location'), 'remote_preferred') \
                    <> 'remote_required' AND \
                COALESCE(json_array_length(json_extract(\
                    d.request, '$.placement.required_credentials')), 0) = 0))";
            let reservation_recovery = format!(
                "SELECT d.run_id FROM {p}_dispatch d \
                 WHERE d.status IN ('reserved', 'reservation_running') \
                 AND d.lease_until IS NOT NULL AND d.lease_until < ?1 \
                 AND {} ORDER BY d.created_at LIMIT 1",
                no_running_peer(p)
            );
            let recovery = format!(
                "SELECT d.run_id FROM {p}_dispatch d \
                 WHERE d.status = 'running' AND d.lease_until IS NOT NULL \
                 AND d.lease_until < ?1 AND {local_eligible} \
                 ORDER BY d.cancel_requested DESC, d.created_at LIMIT 1"
            );
            // ADR-0022 single writer is an open-Run fence: an Awaiting Run keeps
            // ownership until its exact reply/cancel resumes that same row.
            let thread_available = thread_available_for_claim(p);
            let wake = format!(
                "SELECT run_id FROM {p}_dispatch d \
                 WHERE d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
                     SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
                     AND (pe.available_at IS NULL OR pe.available_at <= ?1))) \
                 AND {thread_available} AND {local_eligible} \
                 ORDER BY d.cancel_requested DESC, d.created_at LIMIT 1"
            );
            let fresh = format!(
                "SELECT run_id FROM {p}_dispatch d \
                 WHERE d.status = 'pending' AND {thread_available} AND {local_eligible} \
                 ORDER BY d.cancel_requested DESC, d.priority DESC, d.created_at LIMIT 1"
            );

            let row = |sql: &str, bind_now: bool| -> Result<Option<String>, DispatchError> {
                let map = |r: &rusqlite::Row| r.get::<_, String>(0);
                if bind_now {
                    tx.query_row(sql, params![crate::clock::db_millis(now_ms)], map)
                } else {
                    tx.query_row(sql, [], map)
                }
                .optional()
                .map_err(reject)
            };

            let picked = match row(&reservation_recovery, true)? {
                Some(found) => Some(found),
                None => match row(&recovery, true)? {
                    Some(found) => Some(found),
                    None => match row(&wake, true)? {
                        Some(found) => Some(found),
                        None => row(&fresh, false)?,
                    },
                },
            };

            let Some(run_id) = picked else {
                return Ok(None);
            };
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &owner,
                lease_ms,
                now_ms,
                None,
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let worker = worker.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let thread_available = thread_available_for_claim(p);
            let sql = format!(
                "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
                 (d.status IN ('reserved', 'reservation_running') AND \
                   d.lease_until IS NOT NULL AND d.lease_until < ?1 AND {}) OR \
                 (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?1) OR \
                 (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
                   WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= ?1)) \
                   ) AND {thread_available}) OR \
                 (d.status = 'pending' AND {thread_available}) \
                 ORDER BY CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                          d.priority DESC, d.created_at",
                no_running_peer(p)
            );
            let capabilities = installed_worker_credential_capabilities(&worker)?;
            let selected = {
                let mut stmt = tx.prepare(&sql).map_err(reject)?;
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
                    .map_err(reject)?;
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
                    ) = row.map_err(reject)?;
                    let request: RunDispatch =
                        serde_json::from_str(&request_json).map_err(json_err)?;
                    let previous: Option<WorkerAssignment> = previous_json
                        .map(|value| serde_json::from_str(&value).map_err(json_err))
                        .transpose()?;
                    let next_epoch = next_claim_epoch(lease_epoch)?;
                    if matches!(status.as_str(), "reserved" | "reservation_running")
                        || cancellation_requested
                        || (can_assign(
                            &worker,
                            &request.placement,
                            previous.as_ref(),
                            sandbox.is_some(),
                            now_ms,
                        )
                        .is_ok()
                            && can_admit_attempt_credentials(
                                &request,
                                &capabilities,
                                next_epoch,
                                now_ms,
                            ))
                    {
                        selected = Some(run_id);
                        break;
                    }
                }
                selected
            };
            let Some(run_id) = selected else {
                tx.commit().map_err(reject)?;
                return Ok(None);
            };
            let claimed = claim_exact_transaction(
                &tx,
                &run_id,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let requester = requester.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let thread_available = thread_available_for_claim(p);
            let sql = format!(
                "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
                 (d.status IN ('reserved', 'reservation_running') AND \
                   d.lease_until IS NOT NULL AND d.lease_until < ?1 AND {}) OR \
                 (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?1) OR \
                 (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (SELECT 1 FROM {p}_pending pe \
                   WHERE pe.run_id = d.run_id AND (pe.available_at IS NULL OR pe.available_at <= ?1)) \
                   ) AND {thread_available}) OR \
                 (d.status = 'pending' AND {thread_available}) \
                 ORDER BY CASE WHEN d.cancel_requested = 1 THEN 0 WHEN d.status = 'running' THEN 1 WHEN d.status = 'awaiting' THEN 2 ELSE 3 END, \
                          d.priority DESC, d.created_at",
                no_running_peer(p)
            );
            let capabilities = installed_worker_credential_capabilities(&requester)?;
            let selected = {
                let mut stmt = tx.prepare(&sql).map_err(reject)?;
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
                    .map_err(reject)?;
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
                    ) = row.map_err(reject)?;
                    let request: RunDispatch =
                        serde_json::from_str(&request_json).map_err(json_err)?;
                    let previous: Option<WorkerAssignment> = previous_json
                        .map(|value| serde_json::from_str(&value).map_err(json_err))
                        .transpose()?;
                    let next_epoch = next_claim_epoch(lease_epoch)?;
                    if matches!(status.as_str(), "reserved" | "reservation_running")
                        || cancellation_requested
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
                        )? && can_admit_attempt_credentials(
                            &request,
                            &capabilities,
                            next_epoch,
                            now_ms,
                        ))
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
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let requested_run = requested_run.0.clone();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, _p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let claimed = claim_exact_transaction(
                &tx,
                &requested_run,
                &owner,
                lease_ms,
                now_ms,
                None,
                &capabilities,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_for_terminal_recovery(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let requested_run = requested_run.0.clone();
        let owner = owner.to_string();
        self.with_conn(move |conn, _p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
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
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = crate::clock::normalize_millis(now_ms);
        let owner = owner.to_string();
        self.with_conn(move |conn, p| {
            claim_retry_exhausted_transaction(conn, p, &owner, lease_ms, now_ms, max_attempts)
        })
        .await
    }

    async fn claim_run_compatible(
        &self,
        requested_run: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let requested_run = requested_run.0.clone();
        let worker = worker.clone();
        self.with_conn(move |conn, _p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let claimed = claim_exact_transaction(
                &tx,
                &requested_run,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
                &installed_worker_credential_capabilities(&worker)?,
            )?;
            tx.commit().map_err(reject)?;
            Ok(claimed)
        })
        .await
    }

    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let run_id = run_id.0.clone();
        let owner = owner.to_string();
        self.with_conn(move |conn, p| {
            let n = conn
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET lease_until = ?1 \
                         WHERE run_id = ?2 AND status IN ('running', 'reservation_running') \
                         AND lease_owner = ?3"
                    ),
                    params![
                        crate::clock::db_millis(crate::clock::deadline_millis(now_ms, lease_ms)),
                        run_id,
                        owner
                    ],
                )
                .map_err(reject)?;
            Ok(n > 0)
        })
        .await
    }

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        let sandbox_ref = sandbox_ref.to_string();
        self.with_conn(move |conn, p| {
            let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
            let changed = conn
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET sandbox = ?1 WHERE run_id = ?2 \
                     AND status = 'running' AND lease_owner = ?3 AND lease_epoch = ?4"
                    ),
                    params![sandbox_ref, claim.run_id.0, claim.owner, claim_epoch],
                )
                .map_err(reject)?;
            Ok(if changed == 1 {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            })
        })
        .await
    }

    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        self.with_conn(move |conn, p| {
            let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let current: Option<(Option<String>, Option<String>)> = tx
                .query_row(
                    &format!(
                        "SELECT credential_bindings, credential_receipts FROM {p}_dispatch \
                         WHERE run_id = ?1 AND status = 'running' \
                         AND lease_owner = ?2 AND lease_epoch = ?3"
                    ),
                    params![claim.run_id.0, claim.owner, claim_epoch],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(reject)?;
            let Some((bindings_json, receipts_json)) = current else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let bindings: Vec<AttemptCredentialBinding> = bindings_json
                .map(|value| serde_json::from_str(&value).map_err(json_err))
                .transpose()?
                .unwrap_or_default();
            verify_credential_realization_receipt(&bindings, &receipt)
                .map_err(|error| DispatchError::Rejected(error.to_string()))?;
            let mut receipts: Vec<CredentialRealizationReceipt> = receipts_json
                .map(|value| serde_json::from_str(&value).map_err(json_err))
                .transpose()?
                .unwrap_or_default();
            if let Some(existing) = receipts
                .iter()
                .find(|existing| existing.candidate_fingerprint == receipt.candidate_fingerprint)
            {
                if existing != &receipt {
                    return Err(DispatchError::Rejected(
                        "credential realization receipt conflicts with committed evidence"
                            .to_string(),
                    ));
                }
                tx.commit().map_err(reject)?;
                return Ok(SettleOutcome::Applied);
            }
            receipts.push(receipt);
            let changed = tx
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET credential_receipts = ?1 \
                         WHERE run_id = ?2 AND status = 'running' \
                         AND lease_owner = ?3 AND lease_epoch = ?4"
                    ),
                    params![json(&receipts)?, claim.run_id.0, claim.owner, claim_epoch],
                )
                .map_err(reject)?;
            if changed != 1 {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            tx.commit().map_err(reject)?;
            Ok(SettleOutcome::Applied)
        })
        .await
    }

    async fn runnable_depth(&self, now_ms: u64) -> Result<Option<u64>, DispatchError> {
        self.with_conn(move |conn, p| {
            let thread_available = thread_available_for_claim(p);
            let depth: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {p}_dispatch d WHERE \
                         (d.status IN ('reserved', 'reservation_running') \
                           AND d.lease_until IS NOT NULL AND d.lease_until < ?1 AND {}) OR \
                         (d.status = 'pending' AND {thread_available}) OR \
                         (d.status = 'running' AND d.lease_until < ?1) OR \
                         (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (\
                           SELECT 1 FROM {p}_pending i WHERE i.run_id = d.run_id \
                           AND (i.available_at IS NULL OR i.available_at <= ?1)\
                         )) AND {thread_available})",
                        no_running_peer(p)
                    ),
                    params![crate::clock::db_millis(now_ms)],
                    |row| row.get(0),
                )
                .map_err(reject)?;
            Ok(Some(durable_u64("runnable dispatch depth", depth)?))
        })
        .await
    }

    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let owner = owner.to_string();
        self.with_conn(move |conn, p| {
            // Only rows within half a lease of expiring; a fresh claim is a full
            // length out and is skipped until it approaches expiry (ADR-0024).
            let n = conn
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET lease_until = ?1 \
                         WHERE status IN ('running', 'reservation_running') AND lease_owner = ?2 \
                         AND lease_until IS NOT NULL AND lease_until < ?3"
                    ),
                    params![
                        crate::clock::db_millis(crate::clock::deadline_millis(now_ms, lease_ms)),
                        owner,
                        crate::clock::db_millis(crate::clock::deadline_millis(
                            now_ms,
                            lease_ms / 2
                        ))
                    ],
                )
                .map_err(reject)?;
            Ok(n)
        })
        .await
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        self.with_conn(move |conn, p| {
            let epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let current: Option<(String, Option<String>, i64, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT status, lease_owner, lease_epoch, cancel_requested \
                         FROM {p}_dispatch WHERE run_id = ?1"
                    ),
                    params![claim.run_id.0],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()
                .map_err(reject)?;
            let Some((status, owner, persisted_epoch, cancellation_requested)) = current else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let transition = crate::persisted_dispatch_transition(
                &status,
                persisted_epoch,
                cancellation_requested != 0,
            )?;
            let awaken_run_ingress_contract::GuardedTransition::Applied(next) =
                transition.relinquish(claim.epoch, owner.as_deref() == Some(&claim.owner))
            else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let changed = tx
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET status = ?4, lease_owner = NULL, \
                         lease_until = NULL, created_at = strftime('%Y-%m-%d %H:%M:%f', 'now') \
                         WHERE run_id = ?1 AND status = 'running' \
                         AND lease_owner = ?2 AND lease_epoch = ?3"
                    ),
                    params![
                        claim.run_id.0,
                        claim.owner,
                        epoch,
                        crate::dispatch_state_db(next.state)
                    ],
                )
                .map_err(reject)?;
            if changed != 1 {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            insert_operation(
                &tx,
                p,
                &DispatchOperation::LeaseLost {
                    claim,
                    reason: LeaseLossReason::Relinquished,
                },
            )?;
            tx.commit().map_err(reject)?;
            Ok(SettleOutcome::Applied)
        })
        .await
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let run_id = run_id.0.clone();
        let consumed = consumed.to_vec();
        self.with_conn(move |conn, p| {
            let epoch_i64 = durable_i64("dispatch lease epoch", epoch)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let authority: Option<(String, Option<String>, i64, i64, String)> = tx
                .query_row(
                    &format!(
                        "SELECT status, lease_owner, lease_epoch, cancel_requested, request \
                         FROM {p}_dispatch WHERE run_id = ?1"
                    ),
                    params![run_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(reject)?;
            let Some((status, owner, persisted_epoch, cancellation_requested, request_json)) =
                authority
            else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let transition = crate::persisted_dispatch_transition(
                &status,
                persisted_epoch,
                cancellation_requested != 0,
            )?
            .settle(epoch, outcome == DispatchOutcome::Done);
            if matches!(
                transition,
                awaken_run_ingress_contract::GuardedTransition::Fenced
            ) {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            let Some(owner) = owner else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
            let request_fingerprint = request.canonical_fingerprint();
            let claim = RunClaim {
                run_id: RunId(run_id.clone()),
                owner,
                epoch,
            };
            // Fence first: mutate the dispatch row ONLY while the caller still holds
            // the current epoch. A stale owner (lower epoch) affects zero rows, so
            // its settle touches neither the dispatch nor its pending.
            let dispatch_rows = match transition {
                awaken_run_ingress_contract::GuardedTransition::Removed => tx
                    .execute(
                        &format!(
                            "DELETE FROM {p}_dispatch WHERE run_id = ?1 \
                             AND status = 'running' AND lease_epoch = ?2"
                        ),
                        params![run_id, epoch_i64],
                    )
                    .map_err(reject)?,
                awaken_run_ingress_contract::GuardedTransition::Applied(next) => tx
                    .execute(
                        &format!(
                            "UPDATE {p}_dispatch SET status = ?3, lease_owner = NULL, \
                             lease_until = NULL, attempt_count = 0 \
                             WHERE run_id = ?1 AND status = 'running' AND lease_epoch = ?2"
                        ),
                        params![run_id, epoch_i64, crate::dispatch_state_db(next.state)],
                    )
                    .map_err(reject)?,
                awaken_run_ingress_contract::GuardedTransition::Fenced => unreachable!(),
            };
            if dispatch_rows == 0 {
                // Fenced: re-claimed under a higher epoch (or already gone). Change
                // nothing and report the loss so the stale caller abandons.
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            if outcome == DispatchOutcome::Done {
                tx.execute(
                    &format!(
                        "INSERT INTO {p}_dispatch_completion \
                         (run_id, request_fingerprint, thread_id, session_thread_id) \
                         VALUES (?1, ?2, ?3, ?4) \
                         ON CONFLICT(run_id) DO NOTHING"
                    ),
                    params![
                        run_id,
                        request_fingerprint,
                        request.thread_id().0,
                        request.session_thread_id.as_ref().map(|thread| &thread.0)
                    ],
                )
                .map_err(reject)?;
            }
            // The fence held; now reconcile the run's pending input.
            match outcome {
                DispatchOutcome::Done => {
                    tx.execute(
                        &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                        params![run_id],
                    )
                    .map_err(reject)?;
                    // Also drop anything else consumed this attempt (e.g. unbound
                    // idle-thread input, ADR-0021).
                    for message_id in &consumed {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                            params![message_id],
                        )
                        .map_err(reject)?;
                    }
                }
                DispatchOutcome::Awaiting => {
                    for message_id in &consumed {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                            params![message_id],
                        )
                        .map_err(reject)?;
                    }
                }
            }
            insert_operation(&tx, p, &DispatchOperation::Settled { claim, outcome })?;
            tx.commit().map_err(reject)?;
            Ok(SettleOutcome::Applied)
        })
        .await
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
                .map_err(reject)?;
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
                .map_err(reject)?;
            let mut events = Vec::new();
            for row in rows {
                let (sequence, run_id, request_fingerprint, thread_id, session_thread_id) =
                    row.map_err(reject)?;
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

    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let max_attempts_i64 = durable_i64("dispatch retry limit", max_attempts)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let candidates = {
                let mut statement = tx
                    .prepare(&format!(
                        "SELECT run_id, lease_owner, lease_epoch, attempt_count, cancel_requested \
                         FROM {p}_dispatch WHERE status = 'running' \
                         AND lease_until IS NOT NULL AND lease_until < ?1 \
                         AND attempt_count >= ?2 ORDER BY created_at"
                    ))
                    .map_err(reject)?;
                let rows = statement
                    .query_map(
                        params![crate::clock::db_millis(now_ms), max_attempts_i64],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, i64>(4)?,
                            ))
                        },
                    )
                    .map_err(reject)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(reject)?
            };
            let mut quarantined = 0;
            for (run_id, owner, epoch, attempt_count, cancellation_requested) in candidates {
                let Some(owner) = owner else {
                    return Err(DispatchError::Rejected(
                        "expired running dispatch has no persisted lease owner".to_string(),
                    ));
                };
                let claim_epoch = durable_u64("dispatch lease epoch", epoch)?;
                let current = crate::persisted_dispatch_transition(
                    "running",
                    epoch,
                    cancellation_requested != 0,
                )?;
                let awaken_run_ingress_contract::GuardedTransition::Applied(next) =
                    current.exhaust_retries(claim_epoch, true)
                else {
                    continue;
                };
                let changed = tx
                    .execute(
                        &format!(
                            "UPDATE {p}_dispatch SET status = ?4, \
                             lease_owner = NULL, lease_until = NULL, dead_lettered_at = ?1 \
                             WHERE run_id = ?2 AND status = 'running' AND lease_epoch = ?3"
                        ),
                        params![
                            crate::clock::db_millis(now_ms),
                            run_id,
                            epoch,
                            crate::dispatch_state_db(next.state)
                        ],
                    )
                    .map_err(reject)?;
                if changed == 0 {
                    continue;
                }
                let claim = RunClaim {
                    run_id: RunId(run_id),
                    owner,
                    epoch: claim_epoch,
                };
                insert_operation(
                    &tx,
                    p,
                    &DispatchOperation::LeaseLost {
                        claim: claim.clone(),
                        reason: LeaseLossReason::RetryExhausted,
                    },
                )?;
                insert_operation(
                    &tx,
                    p,
                    &DispatchOperation::DeadLettered {
                        claim,
                        attempt_count: durable_u64("dispatch attempt count", attempt_count)?,
                    },
                )?;
                quarantined += 1;
            }
            tx.commit().map_err(reject)?;
            Ok(quarantined)
        })
        .await
    }

    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("dead_letter").await
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        self.run_ids_by_status("superseded").await
    }

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        self.with_conn(move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT run_id, thread_id, request, status, attempt_count, cancel_requested, sandbox, lease_until FROM {p}_dispatch \
                     ORDER BY created_at"
                ))
                .map_err(reject)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)? != 0,
                        r.get::<_, Option<String>>(6)?.is_some(),
                        r.get::<_, Option<i64>>(7)?,
                    ))
                })
                .map_err(reject)?;
            let mut out = Vec::new();
            for row in rows {
                let (
                    run_id,
                    thread_id,
                    request,
                    status,
                    attempt_count,
                    cancellation_requested,
                    sandbox_bound,
                    lease_until,
                ) = row.map_err(reject)?;
                let request = serde_json::from_str::<RunDispatch>(&request).map_err(json_err)?;
                let state = DispatchState::from_db(&status).ok_or_else(|| {
                    DispatchError::Rejected(format!("unknown persisted dispatch state {status}"))
                })?;
                out.push(DispatchSummary {
                    run_id: RunId(run_id),
                    thread_id: ThreadId(thread_id),
                    session_thread_id: request.session_thread_id,
                    session_activity_epoch: request.session_activity_epoch,
                    reservation_deadline_ms: (state == DispatchState::Reserved)
                        .then(|| lease_until.map(crate::clock::millis_from_db).transpose())
                        .transpose()
                        .map_err(|error| DispatchError::Rejected(error.to_string()))?
                        .flatten(),
                    state,
                    cancellation_requested,
                    attempt_count: durable_u64("dispatch attempt count", attempt_count)?,
                    sandbox_bound,
                });
            }
            Ok(out)
        })
        .await
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let run_id = run_id.0.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let current: Option<(String, i64, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT status, lease_epoch, cancel_requested FROM {p}_dispatch \
                         WHERE run_id = ?1"
                    ),
                    params![run_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(reject)?;
            let Some((status, lease_epoch, cancellation_requested)) = current else {
                tx.commit().map_err(reject)?;
                return Ok(false);
            };
            let Some(next) = crate::persisted_dispatch_transition(
                &status,
                lease_epoch,
                cancellation_requested != 0,
            )?
            .requeue_dead_letter() else {
                tx.commit().map_err(reject)?;
                return Ok(false);
            };
            let n = tx
                .execute(
                    &format!(
                        "UPDATE {p}_dispatch SET status = ?2, attempt_count = 0, \
                         lease_owner = NULL, lease_until = NULL \
                         WHERE run_id = ?1 AND status = 'dead_letter' \
                         AND cancel_requested = 0"
                    ),
                    params![run_id, crate::dispatch_state_db(next.state)],
                )
                .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(n > 0)
        })
        .await
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let run_id = run_id.0.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            let current: Option<(String, String, Option<String>, i64, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT thread_id, status, lease_owner, lease_epoch, cancel_requested FROM {p}_dispatch \
                         WHERE run_id = ?1 AND status IN \
                         ('reserved', 'reservation_running', 'pending', 'awaiting', 'running', 'dead_letter')"
                    ),
                    params![run_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(reject)?;
            if let Some((_, status, _, epoch, cancellation_requested)) = &current {
                let transition = crate::persisted_dispatch_transition(
                    status,
                    *epoch,
                    *cancellation_requested != 0,
                )?;
                let awaken_run_ingress_contract::CancelTransition::Applied {
                    state: next,
                    revoked_lease,
                } = transition.cancel().map_err(crate::transition_error)?
                else {
                    tx.commit().map_err(reject)?;
                    return Ok(None);
                };
                tx.execute(
                    &format!(
                        "UPDATE {p}_dispatch SET cancel_requested = ?2, lease_epoch = ?3, \
                         lease_owner = CASE WHEN ?4 THEN NULL ELSE lease_owner END, \
                         lease_until = CASE WHEN ?4 AND ?5 = 'reserved' THEN 0 \
                           WHEN ?4 THEN NULL ELSE lease_until END, status = ?5 \
                         WHERE run_id = ?1"
                    ),
                    params![
                        run_id,
                        i64::from(next.cancellation_requested),
                        durable_i64("dispatch lease epoch", next.lease_epoch)?,
                        revoked_lease,
                        crate::dispatch_state_db(next.state)
                    ],
                )
                .map_err(reject)?;
                if revoked_lease {
                    let (_, _, owner, epoch, _) = current.as_ref().expect("current exists");
                    insert_operation(
                        &tx,
                        p,
                        &DispatchOperation::LeaseLost {
                            claim: RunClaim {
                                run_id: RunId(run_id.clone()),
                                owner: owner.clone().ok_or_else(|| {
                                    DispatchError::Rejected(
                                        "claimed dispatch has no persisted lease owner".to_string(),
                                    )
                                })?,
                                epoch: u64::try_from(*epoch).map_err(|_| {
                                    DispatchError::Rejected(
                                        "persisted dispatch claim epoch is negative".to_string(),
                                    )
                                })?,
                            },
                            reason: LeaseLossReason::Cancelled,
                        },
                    )?;
                }
            }
            tx.commit().map_err(reject)?;
            Ok(current.map(|(thread, _, _, _, _)| ThreadId(thread)))
        })
        .await
    }

    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        let thread_id = thread_id.0.clone();
        self.with_conn(move |conn, p| {
            let run: Option<String> = conn
                .query_row(
                    &format!(
                        "SELECT run_id FROM {p}_dispatch WHERE thread_id = ?1 AND status = 'awaiting' \
                         AND cancel_requested = 0 \
                         ORDER BY created_at LIMIT 1"
                    ),
                    params![thread_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(reject)?;
            Ok(run.map(RunId))
        })
        .await
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            tx.execute(
                &format!(
                    "DELETE FROM {p}_pending WHERE run_id IN \
                     (SELECT run_id FROM {p}_dispatch WHERE status = 'dead_letter')"
                ),
                [],
            )
            .map_err(reject)?;
            let n = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE status = 'dead_letter'"),
                    [],
                )
                .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(n)
        })
        .await
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let cond = "status = 'dead_letter' AND dead_lettered_at IS NOT NULL \
                        AND dead_lettered_at <= ?1";
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(reject)?;
            tx.execute(
                &format!(
                    "DELETE FROM {p}_pending WHERE run_id IN \
                     (SELECT run_id FROM {p}_dispatch WHERE {cond})"
                ),
                params![crate::clock::db_millis(cutoff_ms)],
            )
            .map_err(reject)?;
            let n = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE {cond}"),
                    params![crate::clock::db_millis(cutoff_ms)],
                )
                .map_err(reject)?;
            tx.commit().map_err(reject)?;
            Ok(n)
        })
        .await
    }
}
