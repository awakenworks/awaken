// @formal entrypoint credential_effect.boundary_preserves_exact_materialization_authority
// @formal entrypoint ingress.pending_input_exactly_once
// @formal entrypoint ingress.settle_fence
// @formal entrypoint remote_worker.pending_signal_is_consumed_by_commit
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal entrypoint run_ingress.renewal_projects_cancel_only_to_exact_physical_attempt
// @formal entrypoint run_ingress.request_local_resolution_retry_honors_persisted_deadline
// @formal entrypoint session.agent_message_completed_target_allows_late_source_receipt_without_reexecution
// @formal entrypoint session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
// @formal entrypoint session_execution.one_physical_attempt_per_thread
// @formal entrypoint session_run.recovery_claim_is_admission_only
// @formal entrypoint session_run.reservation_ttl_uses_store_clock
// @formal entrypoint worker.durable_dispatch_contains_no_plaintext_secret
// @formal evidence ingress.epoch_reclaim
// @formal evidence ingress.single_owner
// SQLite realization of the one neutral DispatchQueue contract. Included by
// `sqlite.rs` so the public adapter and trait implementation paths do not move.
#[async_trait]
impl DispatchQueue for SqliteDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let now_ms = self.clock.now_ms();
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
                .map_err(store_error)?;
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
                .map_err(store_error)?;
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
                .map_err(store_error)?;
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
                    let awaken_dispatch_contract::GuardedTransition::Applied(next) = transition
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
                    .map_err(store_error)?
                }
                SessionRunReservationResolution::Retry { reservation_ttl_ms } => {
                    let awaken_dispatch_contract::GuardedTransition::Applied(next) = transition
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
                    .map_err(store_error)?
                }
                SessionRunReservationResolution::Rejected => {
                    assert!(matches!(
                        transition,
                        awaken_dispatch_contract::GuardedTransition::Removed
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
                        .map_err(store_error)?;
                    if changed == 1 {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                            params![claim.run_id.0],
                        )
                        .map_err(store_error)?;
                    }
                    changed
                }
            };
            if changed != 1 {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            tx.commit().map_err(store_error)?;
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
                .map_err(store_error)?;

            // A replayed completed run is a no-op before supersession, so it
            // cannot mutate sibling rows on its thread (ADR-0060).
            if exact_run_replay(&tx, p, &request)? {
                tx.commit().map_err(store_error)?;
                return Ok(());
            }

            validate_executable_dispatch_admission(&request)?;
            insert_new_dispatch(&tx, p, &request, &options)?;
            tx.commit().map_err(store_error)?;
            Ok(())
        })
        .await
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let run_id = request.run_id().0.clone();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
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
                tx.commit().map_err(store_error)?;
                return Ok(claimed);
            }
            validate_executable_dispatch_admission(&request)?;
            if !can_claim_locally(&request.placement) {
                tx.commit().map_err(store_error)?;
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
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let run_id = request.run_id().0.clone();
        let worker = worker.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
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
                tx.commit().map_err(store_error)?;
                return Ok(claimed);
            }
            validate_executable_dispatch_admission(&request)?;
            if can_assign(&worker, &request.placement, None, false, now_ms).is_err() {
                tx.commit().map_err(store_error)?;
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
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.0.clone();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
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
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let input = normalize_pending_millis(input);
        let run_id = input.run_id.0.clone();
        let worker = worker.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
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
            tx.commit().map_err(store_error)?;
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

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;

            let select_band = |quiescent_only: bool| -> Result<Option<String>, DispatchError> {
                let physical_eligible = if quiescent_only {
                    "d.active_attempt_owner IS NULL AND d.active_attempt_epoch IS NULL"
                } else {
                    "1 = 1"
                };
                // Priority: recover an expired lease, then wake an awaiting run with
                // pending input, then a fresh pending run. SQLite has no SKIP LOCKED;
                // the IMMEDIATE transaction is the single-owner guard.
                let local_eligible = "(d.cancel_requested = 1 OR (\
                    COALESCE(json_extract(d.request, '$.placement.location'), 'remote_preferred') \
                        <> 'remote_required' AND \
                    COALESCE(json_array_length(json_extract(\
                        d.request, '$.placement.required_credentials')), 0) = 0))";
                let retry_due = "(d.retry_not_before IS NULL \
                    OR d.retry_not_before <= ?1)";
                let reservation_recovery = format!(
                    "SELECT d.run_id, d.request, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d \
                     WHERE {physical_eligible} AND d.status IN ('reserved', 'reservation_running') \
                     AND d.lease_until IS NOT NULL AND d.lease_until < ?1 AND {retry_due} \
                    AND {} ORDER BY d.created_at",
                    no_running_peer(p)
                );
                let recovery = format!(
                    "SELECT d.run_id, d.request, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d \
                     WHERE {physical_eligible} AND d.status = 'running' AND d.lease_until IS NOT NULL \
                     AND d.lease_until < ?1 AND {retry_due} AND {local_eligible} \
                     ORDER BY d.cancel_requested DESC, d.created_at"
                );
                // ADR-0022 single writer is an open-Run fence: an Awaiting Run keeps
                // ownership until its exact reply/cancel resumes that same row.
                let thread_available = thread_available_for_claim(p);
                let wake = format!(
                    "SELECT d.run_id, d.request, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d \
                     WHERE {physical_eligible} AND d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
                         SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
                     AND (pe.available_at IS NULL OR pe.available_at <= ?1))) \
                     AND {thread_available} AND {retry_due} AND {local_eligible} \
                     ORDER BY d.cancel_requested DESC, d.created_at"
                );
                let fresh = format!(
                    "SELECT d.run_id, d.request, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d \
                     WHERE {physical_eligible} AND d.status = 'pending' AND {thread_available} \
                     AND {retry_due} AND {local_eligible} \
                     ORDER BY d.cancel_requested DESC, d.priority DESC, d.created_at"
                );

                let row = |sql: &str,
                           bind_now: bool,
                           terminal_resolution: bool|
                 -> Result<Option<String>, DispatchError> {
                    let mut stmt = tx.prepare(sql).map_err(store_error)?;
                    let mut rows = if bind_now {
                        stmt.query(params![crate::clock::db_millis(now_ms)])
                    } else {
                        stmt.query([])
                    }
                    .map_err(store_error)?;
                    while let Some(candidate) = rows.next().map_err(store_error)? {
                        let run_id = candidate.get::<_, String>(0).map_err(store_error)?;
                        let request_json = candidate.get::<_, String>(1).map_err(store_error)?;
                        let cancellation_requested =
                            candidate.get::<_, i64>(2).map_err(store_error)? != 0;
                        let lease_epoch = candidate.get::<_, i64>(3).map_err(store_error)?;
                        let request: RunDispatch =
                            serde_json::from_str(&request_json).map_err(json_err)?;
                        let operation = claim_credential_operation(
                            terminal_resolution,
                            cancellation_requested,
                        );
                        if next_claim_epoch(lease_epoch).is_ok_and(|epoch| {
                            can_admit_claim_credentials(
                                &request,
                                &capabilities,
                                epoch,
                                now_ms,
                                operation,
                            )
                        }) {
                            return Ok(Some(run_id));
                        }
                    }
                    Ok(None)
                };

                let picked = match row(&reservation_recovery, true, true)? {
                    Some(found) => Some(found),
                    None => match row(&recovery, true, false)? {
                        Some(found) => Some(found),
                        None => match row(&wake, true, false)? {
                            Some(found) => Some(found),
                            None => row(&fresh, true, false)?,
                        },
                    },
                };
                Ok(picked)
            };
            let picked = match select_band(true)? {
                Some(run_id) => Some(run_id),
                None => select_band(false)?,
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
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let worker = worker.clone();
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
            let capabilities = installed_worker_credential_capabilities(&worker)?;
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
                        || (can_assign(
                            &worker,
                            &request.placement,
                            previous.as_ref(),
                            sandbox.is_some(),
                            now_ms,
                        )
                        .is_ok()
                            && credentials_admitted)
                    {
                        selected = Some(run_id);
                        break;
                    }
                }
                selected
            };
            let Some(run_id) = selected else {
                tx.commit().map_err(store_error)?;
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
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_run(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let requested_run = requested_run.0.clone();
        let owner = owner.to_string();
        let capabilities = capabilities.clone();
        self.with_conn(move |conn, _p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let claimed = claim_exact_transaction(
                &tx,
                &requested_run,
                &owner,
                lease_ms,
                now_ms,
                None,
                &capabilities,
            )?;
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
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
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let requested_run = requested_run.0.clone();
        let worker = worker.clone();
        self.with_conn(move |conn, _p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let claimed = claim_exact_transaction(
                &tx,
                &requested_run,
                &worker.identity.lease_owner(),
                lease_ms,
                now_ms,
                Some(&worker),
                &installed_worker_credential_capabilities(&worker)?,
            )?;
            tx.commit().map_err(store_error)?;
            Ok(claimed)
        })
        .await
    }

    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        let now_ms = self.clock.now_ms();
        let claim = claim.clone();
        self.with_conn(move |conn, p| {
            let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let current: Option<(
                String,
                Option<String>,
                i64,
                i64,
                Option<String>,
                Option<i64>,
            )> = tx
                .query_row(
                    &format!(
                        "SELECT status, lease_owner, lease_epoch, cancel_requested, \
                         active_attempt_owner, active_attempt_epoch FROM {p}_dispatch \
                         WHERE run_id = ?1"
                    ),
                    params![claim.run_id.0],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(store_error)?;
            let Some((
                status,
                owner,
                persisted_epoch,
                cancellation_requested,
                attempt_owner,
                attempt_epoch,
            )) = current
            else {
                tx.commit().map_err(store_error)?;
                return Ok(LeaseRenewalOutcome::Lost);
            };
            let current_claim = matches!(status.as_str(), "running" | "reservation_running")
                && owner.as_deref() == Some(&claim.owner)
                && persisted_epoch == claim_epoch;
            let cancelled_physical_attempt = cancellation_requested != 0
                && attempt_owner.as_deref() == Some(&claim.owner)
                && attempt_epoch == Some(claim_epoch);
            let outcome = decide_lease_renewal(current_claim, cancelled_physical_attempt);
            if outcome == LeaseRenewalOutcome::Current {
                tx.execute(
                    &format!(
                        "UPDATE {p}_dispatch SET lease_until = ?1 WHERE run_id = ?2 \
                         AND status IN ('running', 'reservation_running') \
                         AND lease_owner = ?3 AND lease_epoch = ?4"
                    ),
                    params![
                        crate::clock::db_millis(crate::clock::deadline_millis(now_ms, lease_ms)),
                        claim.run_id.0,
                        claim.owner,
                        claim_epoch
                    ],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(outcome)
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
                .map_err(store_error)?;
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
                .map_err(store_error)?;
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
                .map_err(store_error)?;
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
            let existing = receipts
                .iter()
                .find(|existing| existing.candidate_fingerprint == receipt.candidate_fingerprint);
            match credential_realization_persistence_decision(existing, &receipt) {
                CredentialRealizationPersistenceDecision::Collision => {
                    return Err(DispatchError::Rejected(
                        "credential realization receipt conflicts with committed evidence"
                            .to_string(),
                    ));
                }
                CredentialRealizationPersistenceDecision::ExactReplay => {
                    tx.commit().map_err(store_error)?;
                    return Ok(SettleOutcome::Applied);
                }
                CredentialRealizationPersistenceDecision::Insert => {}
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
                .map_err(store_error)?;
            if changed != 1 {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            tx.commit().map_err(store_error)?;
            Ok(SettleOutcome::Applied)
        })
        .await
    }

    /// The store's own clock decides liveness, exactly as it does for every
    /// other deadline this backend judges. This used to ride the trait default
    /// on a caller-supplied `now_ms`; the caller no longer has a say in when
    /// "now" is for rows this store owns.
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
        let now_ms = self.clock.now_ms();
        Ok(self
            .lock_commit_epoch(claim)
            .await?
            .is_some_and(|guard| guard.is_live_at(now_ms)))
    }

    async fn begin_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let now_ms = self.clock.now_ms();
        self.admit_physical_attempt(claim, now_ms, false, _binding)
            .await
    }

    async fn handoff_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        let now_ms = self.clock.now_ms();
        self.admit_physical_attempt(claim, now_ms, true, _binding)
            .await
    }

    async fn finish_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        self.with_conn(move |conn, p| {
            let epoch = durable_i64("dispatch attempt epoch", claim.epoch)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let current: Option<(Option<String>, Option<i64>)> = tx
                .query_row(
                    &format!(
                        "SELECT active_attempt_owner, active_attempt_epoch \
                         FROM {p}_dispatch WHERE run_id = ?1"
                    ),
                    params![claim.run_id.0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(store_error)?;
            let Some((active_owner, active_epoch)) = current else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let active_epoch = active_epoch
                .map(|value| durable_u64("dispatch attempt epoch", value))
                .transpose()?;
            let slot = PhysicalAttemptSlot::from_facts(
                active_owner.is_some(),
                active_epoch.is_some(),
                active_owner.as_deref() == Some(&claim.owner),
                active_epoch == Some(claim.epoch),
            );
            let (transition, outcome) = crate::dispatch::decide_physical_attempt_finish(slot)?;
            if transition == PhysicalAttemptTransition::Clear {
                tx.execute(
                    &format!(
                        "UPDATE {p}_dispatch SET active_attempt_owner = NULL, \
                             active_attempt_epoch = NULL, active_attempt_binding = NULL, retired_attempt_epoch = ?3 WHERE run_id = ?1 \
                             AND active_attempt_owner = ?2 AND active_attempt_epoch = ?3"
                    ),
                    params![claim.run_id.0, claim.owner, epoch],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(outcome)
        })
        .await
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        self.with_conn(move |conn, p| release_claim_transaction(conn, p, claim, None))
            .await
    }

    async fn defer_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        let claim = claim.clone();
        let retry_not_before = durable_i64(
            "dispatch resolution retry deadline",
            crate::clock::deadline_millis(self.clock.now_ms(), retry_after_ms),
        )?;
        self.with_conn(move |conn, p| {
            release_claim_transaction(conn, p, claim, Some(retry_not_before))
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
                .map_err(store_error)?;
            let authority: Option<(String, Option<String>, i64, i64, String, Option<i64>, Option<String>)> = tx
                .query_row(
                    &format!(
                        "SELECT status, lease_owner, lease_epoch, cancel_requested, request, active_attempt_epoch, active_attempt_owner \
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
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .optional()
                .map_err(store_error)?;
            let Some((status, owner, persisted_epoch, cancellation_requested, request_json, active_attempt, attempt_owner)) =
                authority
            else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let slot = awaken_dispatch_contract::PhysicalAttemptSlot::from_facts(
                attempt_owner.is_some(), active_attempt.is_some(), false, false,
            );
            let settled_run_id = RunId(run_id.clone());
            let plan = pending_input_settlement_plan(
                crate::persisted_dispatch_transition(
                    &status,
                    persisted_epoch,
                    cancellation_requested != 0,
                )?,
                epoch,
                outcome,
                slot,
                &settled_run_id,
                &consumed,
            );
            if matches!(plan, PendingInputSettlementPlan::Fenced) {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            let Some(owner) = owner else {
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            };
            let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
            let request_fingerprint = request.admission_fingerprint();
            let claim = RunClaim {
                run_id: RunId(run_id.clone()),
                owner,
                epoch,
            };
            // Fence first: mutate the dispatch row ONLY while the caller still holds
            // the current epoch. A stale owner (lower epoch) affects zero rows, so
            // its settle touches neither the dispatch nor its pending.
            let dispatch_rows = match plan {
                PendingInputSettlementPlan::Done { .. } => tx
                    .execute(
                        &format!(
                            "DELETE FROM {p}_dispatch WHERE run_id = ?1 \
                             AND status = 'running' AND lease_epoch = ?2"
                        ),
                        params![run_id, epoch_i64],
                    )
                    .map_err(store_error)?,
                PendingInputSettlementPlan::Awaiting { next, .. } => tx
                    .execute(
                        &format!(
                            "UPDATE {p}_dispatch SET status = ?3, lease_owner = NULL, \
                             lease_until = NULL, attempt_count = 0 \
                             WHERE run_id = ?1 AND status = 'running' AND lease_epoch = ?2"
                        ),
                        params![run_id, epoch_i64, crate::dispatch_state_db(next.state)],
                    )
                    .map_err(store_error)?,
                PendingInputSettlementPlan::Fenced => unreachable!("handled before mutation"),
            };
            if dispatch_rows == 0 {
                // Fenced: re-claimed under a higher epoch (or already gone). Change
                // nothing and report the loss so the stale caller abandons.
                let _ = tx.rollback();
                return Ok(SettleOutcome::Fenced);
            }
            if matches!(plan, PendingInputSettlementPlan::Done { .. }) {
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
                .map_err(store_error)?;
            }
            // The fence held; now reconcile the run's pending input.
            match plan {
                PendingInputSettlementPlan::Done { run_id, consumed } => {
                    tx.execute(
                        &format!("DELETE FROM {p}_pending WHERE run_id = ?1"),
                        params![run_id.0],
                    )
                    .map_err(store_error)?;
                    // Also drop anything else consumed this attempt (e.g. unbound
                    // idle-thread input, ADR-0021).
                    for message_id in consumed {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                            params![message_id],
                        )
                        .map_err(store_error)?;
                    }
                }
                PendingInputSettlementPlan::Awaiting { consumed, .. } => {
                    for message_id in consumed {
                        tx.execute(
                            &format!("DELETE FROM {p}_pending WHERE message_id = ?1"),
                            params![message_id],
                        )
                        .map_err(store_error)?;
                    }
                }
                PendingInputSettlementPlan::Fenced => unreachable!("handled before mutation"),
            }
            insert_operation(&tx, p, &DispatchOperation::Settled { claim, outcome })?;
            tx.commit().map_err(store_error)?;
            Ok(SettleOutcome::Applied)
        })
        .await
    }

    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let max_attempts_i64 = durable_i64("dispatch retry limit", max_attempts)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let candidates = {
                let mut statement = tx
                    .prepare(&format!(
                        "SELECT run_id, lease_owner, lease_epoch, attempt_count, cancel_requested, \
                         active_attempt_owner, active_attempt_epoch \
                         FROM {p}_dispatch WHERE status = 'running' \
                         AND lease_until IS NOT NULL AND lease_until < ?1 \
                         AND attempt_count >= ?2 \
                         AND (active_attempt_epoch IS NULL OR cancel_requested <> 0) \
                         ORDER BY created_at"
                    ))
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(
                        params![crate::clock::db_millis(expired_before_ms), max_attempts_i64],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, i64>(4)?,
                                row.get::<_, Option<String>>(5)?,
                                row.get::<_, Option<i64>>(6)?,
                            ))
                        },
                    )
                    .map_err(store_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(store_error)?
            };
            let mut quarantined = 0;
            for (
                run_id,
                owner,
                epoch,
                attempt_count,
                cancellation_requested,
                active_attempt_owner,
                active_attempt_epoch,
            ) in candidates
            {
                let Some(owner) = owner else {
                    return Err(DispatchError::Rejected(
                        "expired running dispatch has no persisted lease owner".to_string(),
                    ));
                };
                let claim_epoch = durable_u64("dispatch lease epoch", epoch)?;
                // The SQL above only prefilters. The slot disposition is decided
                // here, on the locked row, by the one kernel every backend shares.
                let slot = awaken_dispatch_contract::PhysicalAttemptSlot::from_facts(
                    active_attempt_owner.is_some(),
                    active_attempt_epoch.is_some(),
                    active_attempt_owner.as_deref() == Some(owner.as_str()),
                    active_attempt_epoch == Some(epoch),
                );
                if !slot.permits_settlement()
                    && !slot.quarantine_admits_occupation(cancellation_requested != 0)
                {
                    continue;
                }
                let current = crate::persisted_dispatch_transition(
                    "running",
                    epoch,
                    cancellation_requested != 0,
                )?;
                let awaken_dispatch_contract::GuardedTransition::Applied(next) =
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
                            crate::clock::db_millis(expired_before_ms),
                            run_id,
                            epoch,
                            crate::dispatch_state_db(next.state)
                        ],
                    )
                    .map_err(store_error)?;
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
            tx.commit().map_err(store_error)?;
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
        self.read_dispatch_summaries(None).await
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let run_id = run_id.0.clone();
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
                    params![run_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(store_error)?;
            let Some((status, lease_epoch, cancellation_requested)) = current else {
                tx.commit().map_err(store_error)?;
                return Ok(false);
            };
            let Some(next) = crate::persisted_dispatch_transition(
                &status,
                lease_epoch,
                cancellation_requested != 0,
            )?
            .requeue_dead_letter() else {
                tx.commit().map_err(store_error)?;
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
                .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(n > 0)
        })
        .await
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let run_id = run_id.0.clone();
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
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
                .map_err(store_error)?;
            if let Some((_, status, _, epoch, cancellation_requested)) = &current {
                let transition = crate::persisted_dispatch_transition(
                    status,
                    *epoch,
                    *cancellation_requested != 0,
                )?;
                let awaken_dispatch_contract::CancelTransition::Applied {
                    state: next,
                    revoked_lease,
                    clear_retry_deadline,
                } = transition.cancel().map_err(crate::transition_error)?
                else {
                    tx.commit().map_err(store_error)?;
                    return Ok(None);
                };
                tx.execute(
                    &format!(
                        "UPDATE {p}_dispatch SET cancel_requested = ?2, lease_epoch = ?3, \
                         lease_owner = CASE WHEN ?4 THEN NULL ELSE lease_owner END, \
                         lease_until = CASE WHEN ?4 AND ?5 = 'reserved' THEN 0 \
                           WHEN ?4 THEN NULL ELSE lease_until END, status = ?5, \
                         retry_not_before = CASE WHEN ?6 THEN NULL ELSE retry_not_before END \
                         WHERE run_id = ?1"
                    ),
                    params![
                        run_id,
                        i64::from(next.cancellation_requested),
                        durable_i64("dispatch lease epoch", next.lease_epoch)?,
                        revoked_lease,
                        crate::dispatch_state_db(next.state),
                        clear_retry_deadline,
                    ],
                )
                .map_err(store_error)?;
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
            tx.commit().map_err(store_error)?;
            Ok(current.map(|(thread, _, _, _, _)| ThreadId(thread)))
        })
        .await
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        self.with_conn(move |conn, p| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            tx.execute(
                &format!(
                    "DELETE FROM {p}_pending WHERE run_id IN \
                     (SELECT run_id FROM {p}_dispatch WHERE status = 'dead_letter')"
                ),
                [],
            )
            .map_err(store_error)?;
            let n = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE status = 'dead_letter'"),
                    [],
                )
                .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
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
                .map_err(store_error)?;
            tx.execute(
                &format!(
                    "DELETE FROM {p}_pending WHERE run_id IN \
                     (SELECT run_id FROM {p}_dispatch WHERE {cond})"
                ),
                params![crate::clock::db_millis(cutoff_ms)],
            )
            .map_err(store_error)?;
            let n = tx
                .execute(
                    &format!("DELETE FROM {p}_dispatch WHERE {cond}"),
                    params![crate::clock::db_millis(cutoff_ms)],
                )
                .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(n)
        })
        .await
    }

    fn control(&self) -> Option<&dyn ControlDispatchQueue> {
        Some(self)
    }
}
