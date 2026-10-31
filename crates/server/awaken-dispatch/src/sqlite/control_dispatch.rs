// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: SQLite implementation of the existing ControlDispatchQueue, not a second
// @formal reason: claim or credential authority. Dynamic path: exact owner/epoch/live-lease predicates
// @formal reason: gate credential-binding reads; Session reservations and activation invoke canonical
// @formal reason: classifiers inside BEGIN IMMEDIATE before row/receipt publication. Shared reservation
// @formal reason: and credential conformance own executable evidence; SQL, serde and async refinement are
// @formal reason: not claimed proved.
// SQLite control-only Dispatch authority. Included beside DispatchQueue.

/// One worker-compatible claim candidate: run id, request, sandbox, previous
/// assignment, status, cancellation flag and the persisted lease epoch.
type CompatibleCandidateRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    i64,
);

#[async_trait]
impl awaken_dispatch_contract::DispatchAttemptControl for SqliteDispatchStore {
    async fn finish_attempt_closure(
        &self,
        record: &crate::AttemptClosureRecord,
    ) -> Result<SettleOutcome, DispatchError> {
        self.finish_physical_attempt_with(
            &record.attempt().claim,
            crate::dispatch::AttemptFinish::Independent(record),
        )
        .await
    }

    async fn admit_attempt_guarded(
        &self,
        claim: &RunClaim,
        binding: Option<&AttemptExecutionBinding>,
        guard: awaken_dispatch_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<AttemptAdmission, DispatchError> {
        crate::application::admit_with_incarnation_guard(self, claim, binding, guard, handoff).await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchControlReads for SqliteDispatchStore {
    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &awaken_agent_contract::credential::CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError> {
        let claim = claim.clone();
        let candidate = candidate.clone();
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let claim_epoch = durable_i64("dispatch lease epoch", claim.epoch)?;
        let now_ms = durable_i64("dispatch store time", now_ms)?;
        let bindings_json: Option<Option<String>> = sqlx::query_scalar(&format!(
            "SELECT credential_bindings FROM {p}_dispatch WHERE run_id = ?1 \
             AND status = 'running' AND lease_owner = ?2 AND lease_epoch = ?3 \
             AND lease_until > ?4"
        ))
        .bind(&claim.run_id.0)
        .bind(&claim.owner)
        .bind(claim_epoch)
        .bind(now_ms)
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_error)?;
        let bindings: Vec<AttemptCredentialBinding> = bindings_json
            .flatten()
            .map(|value| serde_json::from_str(&value).map_err(json_err))
            .transpose()?
            .unwrap_or_default();
        Ok(bindings
            .into_iter()
            .find(|binding| binding.candidate_fingerprint == candidate))
    }

    async fn dispatch_depths(&self) -> Result<Option<DispatchDepths>, DispatchError> {
        self.query_dispatch_depths().await
    }

    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<PhysicalAttempt>, DispatchError> {
        let run_id = run_id.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let row: Option<crate::dispatch::PhysicalAttemptStorageRow<String>> =
            sqlx::query_as(&format!(
                "SELECT active_attempt_owner, active_attempt_epoch, active_attempt_binding, request, active_attempt_closing FROM {p}_dispatch WHERE run_id = ?1"
            ))
            .bind(&run_id.0)
            .fetch_optional(&mut *conn)
            .await
            .map_err(store_error)?;
        let Some((owner, epoch, binding, request_json, closing)) = row else {
            return Ok(None);
        };
        let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
        crate::dispatch::decode_physical_attempt(
            &run_id,
            owner,
            epoch
                .map(|v| durable_u64("dispatch attempt epoch", v))
                .transpose()?,
            binding,
            closing,
        )
        .map(|attempt| {
            attempt.map(|mut attempt| {
                attempt.thread_id = Some(request.thread_id().clone());
                attempt.session_thread_id = request.session_thread_id.clone();
                attempt
            })
        })
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
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT sequence, run_id, request_fingerprint, thread_id, session_thread_id, \
             cancel_requested FROM {p}_dispatch_completion \
             WHERE sequence > ?1 ORDER BY sequence LIMIT ?2"
        ))
        .bind(after_sequence)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await
        .map_err(store_error)?;
        rows.iter().map(decode_dispatch_completion).collect()
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
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let clause = if after.is_some() {
            "WHERE run_id > ?2"
        } else {
            ""
        };
        let sql = format!("SELECT run_id FROM {p}_dispatch {clause} ORDER BY run_id LIMIT ?1");
        let mut query = sqlx::query_scalar::<_, String>(&sql).bind(limit);
        if let Some(after) = after {
            query = query.bind(after);
        }
        let ids = query.fetch_all(&mut *conn).await.map_err(store_error)?;
        Ok(ids.into_iter().map(RunId).collect())
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

#[async_trait]
impl awaken_dispatch_contract::SessionRunAdmissionReads for SqliteDispatchStore {
    async fn observe_session_run_admission(
        &self,
        run_id: &RunId,
    ) -> Result<awaken_dispatch_contract::SessionRunAdmissionObservation, DispatchError> {
        use awaken_dispatch_contract::SessionRunAdmissionObservation;
        let (_authority, mut conn) = self.with_conn().await?;
        let row = load_run_identity_evidence(&mut conn, NS, run_id).await?;
        if let Some(request) = decode_live_identity(&row)? {
            crate::dispatch::validate_observed_dispatch_key(
                run_id,
                &ThreadId(row.try_get("live_thread_id").map_err(store_error)?),
                &request,
            )?;
            return Ok(SessionRunAdmissionObservation::Live(Box::new(request)));
        }
        if decode_completed_identity(&row)?.is_some() {
            return Ok(SessionRunAdmissionObservation::Completed(
                decode_dispatch_completion(&row)?,
            ));
        }
        Ok(SessionRunAdmissionObservation::Missing)
    }
}

#[async_trait]
impl awaken_dispatch_contract::SessionRunReservations for SqliteDispatchStore {
    async fn adopt_pending_session_run(
        &self,
        expected_original: RunDispatch,
        command_fingerprint: awaken_session_types::SessionRunCommandFingerprint,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let lease_ms = crate::dispatch::validate_pending_session_run_adoption(
            &expected_original,
            &command_fingerprint,
            owner,
            lease_ms,
        )?;
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let row = sqlx::query(&format!(
            "SELECT request, status, lease_epoch, lease_owner, lease_until, cancel_requested, \
             worker_assignment, credential_bindings, credential_receipts, \
             active_attempt_owner, active_attempt_epoch, active_attempt_binding, active_attempt_closing \
             FROM {p}_dispatch WHERE run_id = ?1"
        ))
        .bind(&expected_original.run_id().0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        };
        let stored: RunDispatch =
            serde_json::from_str(&row.try_get::<String, _>("request").map_err(store_error)?)
                .map_err(json_err)?;
        let status: String = row.try_get("status").map_err(store_error)?;
        let epoch: i64 = row.try_get("lease_epoch").map_err(store_error)?;
        let cancelled: i64 = row.try_get("cancel_requested").map_err(store_error)?;
        let lease_owner: Option<String> = row.try_get("lease_owner").map_err(store_error)?;
        let lease_until: Option<i64> = row.try_get("lease_until").map_err(store_error)?;
        let assignment: Option<String> = row.try_get("worker_assignment").map_err(store_error)?;
        let bindings: Option<String> = row.try_get("credential_bindings").map_err(store_error)?;
        let receipts: Option<String> = row.try_get("credential_receipts").map_err(store_error)?;
        let bindings = bindings
            .map(|value| {
                serde_json::from_str::<Vec<AttemptCredentialBinding>>(&value).map_err(json_err)
            })
            .transpose()?
            .unwrap_or_default();
        let receipts = receipts
            .map(|value| {
                serde_json::from_str::<Vec<CredentialRealizationReceipt>>(&value).map_err(json_err)
            })
            .transpose()?
            .unwrap_or_default();
        let physical_owner: Option<String> =
            row.try_get("active_attempt_owner").map_err(store_error)?;
        let physical_epoch: Option<i64> =
            row.try_get("active_attempt_epoch").map_err(store_error)?;
        let physical_binding: Option<String> =
            row.try_get("active_attempt_binding").map_err(store_error)?;
        let closing: i64 = row.try_get("active_attempt_closing").map_err(store_error)?;
        crate::dispatch::observed_physical_attempt_is_present(
            physical_owner.is_some(),
            physical_epoch.is_some(),
            physical_binding.is_some(),
            closing != 0,
        )?;
        let adopted = crate::dispatch::pending_session_run_adoption(
            &stored,
            &expected_original,
            crate::persisted_dispatch_transition(&status, epoch, cancelled != 0)?,
            lease_owner.is_none()
                && lease_until.is_none()
                && assignment.is_none()
                && bindings.is_empty()
                && receipts.is_empty(),
            PhysicalAttemptSlot::from_facts(
                physical_owner.is_some(),
                physical_epoch.is_some(),
                false,
                false,
            ),
            closing != 0,
            command_fingerprint,
        )?;
        let Some((request, next)) = adopted else {
            tx.commit().await.map_err(store_error)?;
            return Ok(None);
        };
        let epoch = durable_i64("dispatch lease epoch", next.lease_epoch)?;
        let deadline = crate::clock::deadline_millis(self.clock.now_ms(), lease_ms);
        sqlx::query(&format!(
            "UPDATE {p}_dispatch SET request = ?1, status = 'reservation_running', \
             lease_owner = ?2, lease_until = ?3, lease_epoch = ?4 WHERE run_id = ?5"
        ))
        .bind(json(&request)?)
        .bind(owner)
        .bind(crate::clock::db_millis(deadline))
        .bind(epoch)
        .bind(&request.run_id().0)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(Some(RunClaim {
            run_id: request.run_id().clone(),
            owner: owner.to_string(),
            epoch: next.lease_epoch,
        }))
    }

    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        let (request, reservation_ttl_ms) =
            validate_session_run_reservation_request(request, reservation_ttl_ms)?;
        let deadline = crate::clock::deadline_millis(self.clock.now_ms(), reservation_ttl_ms);
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let evidence = load_run_identity_evidence(&mut tx, p, request.run_id()).await?;
        if evidence
            .try_get::<Option<String>, _>("live_run_id")
            .map_err(store_error)?
            .is_some()
        {
            let status: String = evidence.try_get("status").map_err(store_error)?;
            let stored = decode_live_dispatch(&evidence)?;
            let state = DispatchState::from_db(&status).ok_or_else(|| {
                DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
            })?;
            let outcome = classify_live_session_run_reservation(&stored, state, &request);
            tx.commit().await.map_err(store_error)?;
            return Ok(outcome);
        }
        let completed = decode_completed_identity(&evidence)?;
        if let Some(request_fingerprint) = completed {
            let outcome = classify_completed_session_run_reservation(
                request_fingerprint.as_deref(),
                &request,
            );
            tx.commit().await.map_err(store_error)?;
            return Ok(outcome);
        } else {
            let options = SubmitOptions {
                supersede: request.session_run_replacement.supersedes_prior(),
                ..Default::default()
            };
            insert_dispatch_with_state(
                &mut tx,
                p,
                &request,
                &options,
                "reserved",
                Some(crate::clock::db_millis(deadline)),
            )
            .await?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(SessionRunReservationOutcome::Reserved)
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        let run_id = run_id.clone();
        let session_thread_id = session_thread_id.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let current: Option<(String, String, i64, i64)> = sqlx::query_as(&format!(
            "SELECT status, request, lease_epoch, cancel_requested \
             FROM {p}_dispatch WHERE run_id = ?1"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let Some((status, request_json, lease_epoch, cancellation_requested)) = current else {
            let completed = sqlx::query(&format!(
                "SELECT 1 FROM {p}_dispatch_completion WHERE run_id = ?1"
            ))
            .bind(&run_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_error)?
            .is_some();
            let outcome = classify_session_run_reservation_activation(
                None,
                completed,
                &session_thread_id,
                session_activity_epoch,
            )?
            .expect("missing reservation always has a closed outcome");
            tx.commit().await.map_err(store_error)?;
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
            let changed = sqlx::query(&format!(
                "UPDATE {p}_dispatch SET request = ?1, status = ?3, \
                 lease_owner = NULL, lease_until = NULL WHERE run_id = ?2 \
                 AND status = 'reserved'"
            ))
            .bind(json(&request)?)
            .bind(&run_id.0)
            .bind(crate::dispatch_state_db(next.state))
            .execute(&mut *tx)
            .await
            .map_err(store_error)?
            .rows_affected();
            if changed == 1 {
                SessionRunReservationActivation::Activated
            } else {
                SessionRunReservationActivation::RecoveryClaimed
            }
        };
        tx.commit().await.map_err(store_error)?;
        Ok(outcome)
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let run_id = run_id.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let current: Option<(String, i64, i64)> = sqlx::query_as(&format!(
            "SELECT status, lease_epoch, cancel_requested FROM {p}_dispatch \
             WHERE run_id = ?1"
        ))
        .bind(&run_id.0)
        .fetch_optional(&mut *tx)
        .await
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
            tx.commit().await.map_err(store_error)?;
            return Ok(false);
        }
        let changed = sqlx::query(&format!(
            "DELETE FROM {p}_dispatch WHERE run_id = ?1 AND status = 'reserved'"
        ))
        .bind(&run_id.0)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?
        .rows_affected();
        if changed == 1 {
            sqlx::query(&format!("DELETE FROM {p}_pending WHERE run_id = ?1"))
                .bind(&run_id.0)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(changed == 1)
    }

    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        if exact_run_replay(&mut tx, p, &request).await? {
            tx.commit().await.map_err(store_error)?;
            return Ok(());
        }
        let parent = session_child_parent(&request)?.clone();
        ensure_session_child_capacity(
            &request,
            &admission,
            known_session_child_threads(&mut tx, p, &parent).await?,
        )?;
        insert_new_dispatch(&mut tx, p, &request, &SubmitOptions::default()).await?;
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::SessionReservationAuthority for SqliteDispatchStore {
    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.lock_claim_epoch_in_status(claim, "reservation_running")
            .await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchControlClaims for SqliteDispatchStore {
    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let requester = requester.clone();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let mut tx = immediate(&mut conn).await?;
        let thread_available = thread_available_for_claim(p);
        let sql = format!(
            "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, d.status, d.cancel_requested, d.lease_epoch FROM {p}_dispatch d WHERE \
             ((d.status IN ('reserved', 'reservation_running') AND \
               d.lease_until IS NOT NULL AND d.lease_until <= ?1 AND {}) OR \
             (d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until <= ?1) OR \
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
            let mut rows = sqlx::query_as::<_, CompatibleCandidateRow>(&sql)
                .bind(crate::clock::db_millis(now_ms))
                .fetch(&mut *tx);
            let mut selected = None;
            while let Some(row) = rows.try_next().await.map_err(store_error)? {
                let (
                    run_id,
                    request_json,
                    sandbox,
                    previous_json,
                    status,
                    cancellation_requested,
                    lease_epoch,
                ) = row;
                let cancellation_requested = cancellation_requested != 0;
                let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
                if request
                    .worker_codec_admission(Some(&requester.manifest))
                    .is_err()
                {
                    continue;
                }
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
            &mut tx,
            &run_id,
            &requester.identity.lease_owner(),
            lease_ms,
            now_ms,
            Some(&requester),
            &capabilities,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
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
        let (_authority, mut conn) = self.with_conn().await?;
        let mut tx = immediate(&mut conn).await?;
        let claimed = claim_exact_transaction_with_mode(
            &mut tx,
            &requested_run,
            &owner,
            lease_ms,
            now_ms,
            None,
            &Default::default(),
            ExactClaimMode::TerminalRecovery,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }
}
