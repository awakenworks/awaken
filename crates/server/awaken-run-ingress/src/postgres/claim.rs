// @formal entrypoint delegation.independent_child_scheduling_lease
// @formal entrypoint ingress.epoch_reclaim
// @formal entrypoint ingress.single_owner
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.request_local_resolution_retry_honors_persisted_deadline
// @formal entrypoint worker.retry_preserves_pinned_binding
// PostgreSQL exact-claim transaction kernels. These remain private helpers of
// the parent adapter and do not introduce a second claim policy.

/// One read-only discovery order shared by local, compatible, and placed claims.
/// The exact transaction below remains the only mutation/eligibility authority.
const CANDIDATE_PAGE_SIZE: i64 = 128;

#[derive(Clone, Copy)]
enum CandidateAudience<'a> {
    Local,
    Compatible(&'a WorkerSnapshot),
    Placed {
        requester: &'a WorkerSnapshot,
        workers: &'a [WorkerSnapshot],
        policy: &'a dyn PlacementPolicy,
    },
}

impl<'a> CandidateAudience<'a> {
    fn local(self) -> bool {
        matches!(self, Self::Local)
    }

    fn worker(self) -> Option<&'a WorkerSnapshot> {
        match self {
            Self::Local => None,
            Self::Compatible(worker) => Some(worker),
            Self::Placed { requester, .. } => Some(requester),
        }
    }
}

fn candidate_local_filter() -> &'static str {
    "(d.cancel_requested = 1 OR (\
        COALESCE(d.request #>> '{placement,location}', 'remote_preferred') \
            <> 'remote_required' AND \
        jsonb_array_length(COALESCE(\
            d.request #> '{placement,required_credentials}', '[]'::jsonb)) = 0))"
}

fn candidate_state_rank(local: bool) -> &'static str {
    if local {
        "CASE WHEN d.cancel_requested = 1 THEN 0 \
         WHEN d.status IN ('reserved', 'reservation_running') THEN 1 \
         WHEN d.status = 'running' THEN 2 \
         WHEN d.status = 'awaiting' THEN 3 ELSE 4 END"
    } else {
        "CASE WHEN d.cancel_requested = 1 THEN 0 \
         WHEN d.status = 'running' THEN 1 \
         WHEN d.status = 'awaiting' THEN 2 ELSE 3 END"
    }
}

fn candidate_retry_due() -> &'static str {
    "(d.retry_not_before IS NULL OR d.retry_not_before <= $1)"
}

/// An exhausted epoch cannot pass the existing discovery admission check.
/// Prune that permanent runnable front before decoding candidate payloads.
/// Reservation recovery retains its existing first-page handling; other
/// malformed epochs still reach the canonical exact admission error.
fn candidate_epoch_not_exhausted() -> &'static str {
    "d.lease_epoch < 9223372036854775807"
}

/// Produce only one bounded page of payloads. The cheap, status-based inner
/// selection precedes the per-Thread/JSON predicates: a large fresh queue need
/// not evaluate those predicates for every row before its first candidate.
/// The outer flags skip ineligible page members;
/// the exact transaction remains the sole mutation/eligibility authority.
fn candidate_page_sql(local: bool) -> String {
    let p = NS;
    let thread_available = thread_available_for_claim(p);
    let local_filter = if local {
        candidate_local_filter()
    } else {
        "TRUE"
    };
    let reservation_peer = no_running_peer(p);
    let state_rank = candidate_state_rank(local);
    let retry_due = candidate_retry_due();
    let epoch_not_exhausted = candidate_epoch_not_exhausted();
    format!(
        "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, \
                d.status, d.cancel_requested, d.lease_epoch, \
                d.physical_rank, d.state_rank, d.priority, \
                CASE WHEN d.status IN ('reserved', 'reservation_running') \
                     THEN {reservation_peer} \
                     WHEN d.status IN ('pending', 'awaiting') \
                     THEN {thread_available} ELSE TRUE END AS thread_eligible, \
                CASE WHEN d.status IN ('reserved', 'reservation_running') \
                     THEN TRUE ELSE {local_filter} END AS local_eligible \
         FROM (SELECT ranked.* FROM ( \
             SELECT d.run_id, d.thread_id, d.request, d.sandbox, d.worker_assignment, \
                    d.status, d.cancel_requested, d.lease_epoch, \
                    d.priority, d.created_at, \
                    (d.active_attempt_owner IS NOT NULL OR \
                     d.active_attempt_epoch IS NOT NULL)::integer AS physical_rank, \
                    {state_rank} AS state_rank \
             FROM {p}_dispatch d WHERE \
             ((d.status IN ('reserved', 'reservation_running') AND \
               d.lease_until IS NOT NULL AND d.lease_until <= $1) OR \
              (d.status = 'running' AND d.lease_until IS NOT NULL AND \
               d.lease_until <= $1) OR \
              (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
               SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id AND \
               (pe.available_at IS NULL OR pe.available_at <= $1)))) OR \
              d.status = 'pending') \
             AND {retry_due} \
             AND (d.status IN ('reserved', 'reservation_running') OR {epoch_not_exhausted}) \
         ) ranked \
             ORDER BY ranked.physical_rank, ranked.state_rank, \
                      ranked.priority DESC, ranked.created_at, ranked.run_id \
             LIMIT $2 \
         ) d ORDER BY d.physical_rank, d.state_rank, d.priority DESC, \
                      d.created_at, d.run_id"
    )
}

/// Once the first page proves a long incompatible prefix, use one streaming
/// pass with the original database-side Thread/local pruning. It retains the
/// same candidate order and never materializes the whole result in Rust.
fn candidate_deep_sql(local: bool) -> String {
    let p = NS;
    let thread_available = thread_available_for_claim(p);
    let reservation_peer = no_running_peer(p);
    let local_filter = if local {
        candidate_local_filter()
    } else {
        "TRUE"
    };
    let state_rank = candidate_state_rank(local);
    let retry_due = candidate_retry_due();
    let epoch_not_exhausted = candidate_epoch_not_exhausted();
    format!(
        "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, \
                d.status, d.cancel_requested, d.lease_epoch, \
                TRUE AS thread_eligible, TRUE AS local_eligible \
         FROM {p}_dispatch d WHERE \
         ((d.status IN ('reserved', 'reservation_running') AND \
           d.lease_until IS NOT NULL AND d.lease_until <= $1 AND {reservation_peer}) OR \
          (d.status = 'running' AND d.lease_until IS NOT NULL AND \
           d.lease_until <= $1 AND {local_filter}) OR \
          (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
           SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id AND \
           (pe.available_at IS NULL OR pe.available_at <= $1))) AND \
           {thread_available} AND {local_filter}) OR \
          (d.status = 'pending' AND {thread_available} AND {local_filter})) \
         AND {retry_due} AND {epoch_not_exhausted} \
         ORDER BY CASE WHEN d.active_attempt_owner IS NOT NULL OR \
                       d.active_attempt_epoch IS NOT NULL THEN 1 ELSE 0 END, \
                  {state_rank}, d.priority DESC, d.created_at, d.run_id"
    )
}

fn candidate_is_admissible(
    row: &sqlx::postgres::PgRow,
    audience: CandidateAudience<'_>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    now_ms: u64,
) -> Result<bool, DispatchError> {
    let thread_eligible: bool = row.try_get("thread_eligible").map_err(store_error)?;
    let local_eligible: bool = row.try_get("local_eligible").map_err(store_error)?;
    if !thread_eligible || !local_eligible {
        return Ok(false);
    }
    let Json(request): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
    let status: String = row.try_get("status").map_err(store_error)?;
    let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(store_error)?;
    let lease_epoch: i64 = row.try_get("lease_epoch").map_err(store_error)?;
    let terminal_resolution = matches!(status.as_str(), "reserved" | "reservation_running");
    let operation = claim_credential_operation(terminal_resolution, cancellation_requested != 0);
    let credentials_admitted = next_claim_epoch(lease_epoch).is_ok_and(|epoch| {
        can_admit_claim_credentials(&request, capabilities, epoch, now_ms, operation)
    });
    match audience {
        CandidateAudience::Local => Ok(credentials_admitted),
        CandidateAudience::Compatible(worker) => {
            let sandbox: Option<String> = row.try_get("sandbox").map_err(store_error)?;
            let previous: Option<Json<WorkerAssignment>> =
                row.try_get("worker_assignment").map_err(store_error)?;
            Ok(terminal_resolution
                || (cancellation_requested != 0 && credentials_admitted)
                || (can_assign(
                    worker,
                    &request.placement,
                    previous.as_ref().map(|value| &value.0),
                    sandbox.is_some(),
                    now_ms,
                )
                .is_ok()
                    && credentials_admitted))
        }
        CandidateAudience::Placed {
            requester,
            workers,
            policy,
        } => {
            let sandbox: Option<String> = row.try_get("sandbox").map_err(store_error)?;
            let previous: Option<Json<WorkerAssignment>> =
                row.try_get("worker_assignment").map_err(store_error)?;
            Ok(terminal_resolution
                || (cancellation_requested != 0 && credentials_admitted)
                || (policy_selects_requester(
                    &request,
                    policy,
                    DispatchPlacement {
                        recovered: status == "running",
                        previous: previous.as_ref().map(|value| &value.0),
                        sandbox_bound: sandbox.is_some(),
                        requester: &requester.identity,
                        workers,
                        now_ms,
                    },
                )? && credentials_admitted))
        }
    }
}

impl PostgresDispatchStore {
    async fn try_claim_discovered(
        &self,
        run_id: String,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
        audience: CandidateAudience<'_>,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            ThreadLock::SkipBusy,
            &RunId(run_id),
            owner,
            lease_ms,
            audience.worker(),
            capabilities,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }

    async fn claim_discovered(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
        audience: CandidateAudience<'_>,
    ) -> Result<Option<Claimed>, DispatchError> {
        let now_ms = crate::postgres_helpers::postgres_now_ms(&self.pool).await?;
        let first_sql = candidate_page_sql(audience.local());
        let first = sqlx::query(&first_sql)
            .bind(crate::clock::db_millis(now_ms))
            .bind(CANDIDATE_PAGE_SIZE)
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        let first_len = first.len();
        let mut attempted = std::collections::HashSet::new();
        for row in first {
            if !candidate_is_admissible(&row, audience, capabilities, now_ms)? {
                continue;
            }
            let run_id: String = row.try_get("run_id").map_err(store_error)?;
            attempted.insert(run_id.clone());
            if let Some(claimed) = self
                .try_claim_discovered(run_id, owner, lease_ms, capabilities, audience)
                .await?
            {
                return Ok(Some(claimed));
            }
        }
        if first_len < CANDIDATE_PAGE_SIZE as usize {
            return Ok(None);
        }

        // The first page had no winner. Scan the complete read-only candidate
        // order once, with the original database-side Thread/local pruning.
        // Streaming bounds client memory even when every Worker-specific
        // candidate is incompatible. Release its connection before any exact
        // claim, so a one-connection pool cannot deadlock against itself.
        let deep_sql = candidate_deep_sql(audience.local());
        loop {
            let mut stream = sqlx::query(&deep_sql)
                .bind(crate::clock::db_millis(now_ms))
                .fetch(&self.pool);
            let mut selected = None;
            while let Some(row) = stream.try_next().await.map_err(store_error)? {
                let run_id: String = row.try_get("run_id").map_err(store_error)?;
                if attempted.contains(&run_id)
                    || !candidate_is_admissible(&row, audience, capabilities, now_ms)?
                {
                    continue;
                }
                selected = Some(run_id);
                break;
            }
            drop(stream);
            let Some(run_id) = selected else {
                return Ok(None);
            };
            attempted.insert(run_id.clone());
            if let Some(claimed) = self
                .try_claim_discovered(run_id, owner, lease_ms, capabilities, audience)
                .await?
            {
                return Ok(Some(claimed));
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ThreadLock {
    Wait,
    SkipBusy,
}

async fn lock_dispatch_thread(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &RunId,
    mode: ThreadLock,
) -> Result<Option<String>, DispatchError> {
    let thread: Option<String> = sqlx::query_scalar(&format!(
        "SELECT thread_id FROM {NS}_dispatch WHERE run_id = $1"
    ))
    .bind(&run_id.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_error)?;
    if let Some(thread) = &thread {
        // One Thread-before-row ordering for claim and physical admission.
        // Hash collisions only over-serialize; unique indexes remain backstops.
        match mode {
            ThreadLock::Wait => {
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                    .bind(thread)
                    .execute(&mut **tx)
                    .await
                    .map_err(store_error)?;
            }
            ThreadLock::SkipBusy => {
                let acquired: bool =
                    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
                        .bind(thread)
                        .fetch_one(&mut **tx)
                        .await
                        .map_err(store_error)?;
                if !acquired {
                    return Ok(None);
                }
            }
        }
    }
    Ok(thread)
}

async fn claim_exact_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    thread_lock: ThreadLock,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    let now_ms = crate::postgres_helpers::postgres_now_ms(&mut **tx).await?;
    let phase = sqlx::query(&format!(
        "SELECT status, lease_until FROM {NS}_dispatch WHERE run_id = $1"
    ))
    .bind(&requested_run.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_error)?;
    let mode = if let Some(row) = phase {
        let status = row.try_get::<String, _>("status").map_err(store_error)?;
        let state = DispatchState::from_db(&status).ok_or_else(|| {
            DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
        })?;
        let deadline = row
            .try_get::<Option<i64>, _>("lease_until")
            .map_err(store_error)?
            .map(crate::clock::millis_from_db)
            .transpose()
            .map_err(|error| DispatchError::Rejected(error.to_string()))?;
        classify_exact_claim_mode(state, deadline, now_ms)
    } else {
        ExactClaimMode::Runnable
    };
    claim_exact_transaction_with_mode(
        tx,
        thread_lock,
        requested_run,
        owner,
        lease_ms,
        worker,
        capabilities,
        mode,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn claim_exact_transaction_with_mode(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    thread_lock: ThreadLock,
    requested_run: &RunId,
    owner: &str,
    lease_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    mode: ExactClaimMode,
) -> Result<Option<Claimed>, DispatchError> {
    let p = NS;
    let Some(_) = lock_dispatch_thread(tx, requested_run, thread_lock).await? else {
        return Ok(None);
    };
    // All exact-claim modes share the database authority clock. Read it only
    // after the potentially blocking Thread lock: neither caller skew nor
    // time spent awaiting a peer may mint an already-expired new lease.
    let now_ms = crate::postgres_helpers::postgres_now_ms(&mut **tx).await?;
    let thread_available = thread_available_for_claim(p);
    let no_running_peer = no_running_peer(p);
    let eligibility = match mode {
        ExactClaimMode::Runnable => format!(
            "((d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until <= $2) \
             OR (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
               SELECT 1 FROM {p}_pending pe WHERE pe.run_id = d.run_id \
               AND (pe.available_at IS NULL OR pe.available_at <= $2))) AND {thread_available}) \
             OR (d.status = 'pending' AND {thread_available})) \
             AND (d.retry_not_before IS NULL OR d.retry_not_before <= $2) \
             AND $3::bigint IS NULL"
        ),
        ExactClaimMode::ReservationRecovery => format!(
            "((d.status = 'reserved' AND d.lease_until IS NOT NULL \
             AND d.lease_until <= $2) \
             OR (d.status = 'reservation_running' AND d.lease_until IS NOT NULL \
             AND d.lease_until <= $2)) AND {no_running_peer} \
             AND (d.retry_not_before IS NULL OR d.retry_not_before <= $2) \
             AND $3::bigint IS NULL"
        ),
        ExactClaimMode::TerminalRecovery => {
            format!(
                "d.active_attempt_owner IS NULL AND d.active_attempt_epoch IS NULL AND \
                 ((d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until <= $2) \
                 OR (d.status IN ('pending', 'awaiting') \
                 AND d.lease_owner IS NULL AND d.lease_until IS NULL \
                 AND {no_running_peer})) AND $3::bigint IS NULL"
            )
        }
        ExactClaimMode::RetryExhausted { .. } => {
            "d.status = 'running' AND d.lease_until IS NOT NULL \
             AND d.lease_until <= $2 AND d.attempt_count >= $3"
                .to_string()
        }
    };
    let sql = format!(
        "SELECT d.request, d.sandbox, d.status, d.worker_assignment, d.cancel_requested, \
                d.lease_owner, d.lease_epoch, d.lease_until, d.attempt_count \
         FROM {p}_dispatch d \
         WHERE d.run_id = $1 AND ({eligibility}) FOR UPDATE SKIP LOCKED LIMIT 1"
    );
    let retry_limit = mode
        .retry_limit()
        .map(|limit| durable_i64("dispatch retry limit", limit))
        .transpose()?;
    let Some(row) = sqlx::query(&sql)
        .bind(&requested_run.0)
        .bind(crate::clock::db_millis(now_ms))
        .bind(retry_limit)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_error)?
    else {
        return Ok(None);
    };
    let Json(request): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
    let previous: Option<Json<WorkerAssignment>> =
        row.try_get("worker_assignment").map_err(store_error)?;
    let sandbox: Option<String> = row.try_get("sandbox").map_err(store_error)?;
    let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(store_error)?;
    let previous_owner: Option<String> = row.try_get("lease_owner").map_err(store_error)?;
    let previous_epoch: i64 = row.try_get("lease_epoch").map_err(store_error)?;
    if let ExactClaimMode::RetryExhausted { max_attempts } = mode {
        let status = row.try_get::<String, _>("status").map_err(store_error)?;
        let lease_until = row
            .try_get::<Option<i64>, _>("lease_until")
            .map_err(store_error)?;
        let attempt_count = row
            .try_get::<i64, _>("attempt_count")
            .map_err(store_error)?;
        if !retry_exhaustion_evidence_is_eligible(
            &status,
            lease_until,
            attempt_count,
            max_attempts,
            now_ms,
        )? {
            return Ok(None);
        }
    }
    let terminal_resolution = mode.bypasses_execution_admission();
    if !terminal_resolution
        && cancellation_requested == 0
        && match worker {
            Some(worker) => can_assign(
                worker,
                &request.placement,
                previous.as_ref().map(|value| &value.0),
                sandbox.is_some(),
                now_ms,
            )
            .is_err(),
            None => !can_claim_locally(&request.placement),
        }
    {
        return Ok(None);
    }
    let status: String = row.try_get("status").map_err(store_error)?;
    let current_transition =
        crate::persisted_dispatch_transition(&status, previous_epoch, cancellation_requested != 0)?;
    let (claimed_transition, request) = claim_with_frozen(current_transition, request, mode)
        .map_err(crate::transition_error)?
        .ok_or_else(|| {
            DispatchError::Rejected(format!(
                "persisted dispatch state `{status}` is not claimable in {mode:?} mode"
            ))
        })?;
    let claim_epoch = claimed_transition.lease_epoch;
    let credential_operation =
        claim_credential_operation(terminal_resolution, cancellation_requested != 0);
    let credential_bindings = compile_claim_credential_bindings(
        &request,
        capabilities,
        claim_epoch,
        now_ms,
        credential_operation,
    )
    .map_err(|error| {
        DispatchError::Rejected(format!("credential claim admission failed: {error}"))
    })?;
    let expires = crate::clock::deadline_millis(now_ms, lease_ms);
    let claimed_status = crate::dispatch_state_db(claimed_transition.state);
    sqlx::query(&format!(
        "UPDATE {p}_dispatch SET status = $9, lease_owner = $1, lease_until = $2, \
         attempt_count = attempt_count + $3, lease_epoch = $5, worker_assignment = $6, \
         credential_bindings = $7, credential_receipts = $8, retry_not_before = NULL \
         WHERE run_id = $4"
    ))
    .bind(owner)
    .bind(crate::clock::db_millis(expires))
    .bind(i64::from(status == "running"))
    .bind(&requested_run.0)
    .bind(i64::try_from(claim_epoch).map_err(|_| {
        DispatchError::Rejected(
            "dispatch claim epoch exceeds the Postgres authority range".to_string(),
        )
    })?)
    .bind(
        (!terminal_resolution)
            .then(|| worker.map(WorkerAssignment::from))
            .flatten()
            .map(Json),
    )
    .bind(Json(&credential_bindings))
    .bind(Json(Vec::<CredentialRealizationReceipt>::new()))
    .bind(claimed_status)
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    let claim = RunClaim {
        run_id: requested_run.clone(),
        owner: owner.to_string(),
        epoch: claim_epoch,
    };
    if matches!(status.as_str(), "running" | "reservation_running") {
        let previous = RunClaim {
            run_id: requested_run.clone(),
            owner: previous_owner.ok_or_else(|| {
                DispatchError::Rejected(
                    "expired running dispatch has no persisted lease owner".to_string(),
                )
            })?,
            epoch: u64::try_from(previous_epoch).map_err(|_| {
                DispatchError::Rejected("persisted dispatch claim epoch is negative".to_string())
            })?,
        };
        let reason = if matches!(mode, ExactClaimMode::RetryExhausted { .. }) {
            LeaseLossReason::RetryExhausted
        } else {
            LeaseLossReason::Expired
        };
        insert_operation(
            tx,
            &DispatchOperation::LeaseLost {
                claim: previous.clone(),
                reason,
            },
        )
        .await?;
        insert_operation(
            tx,
            &DispatchOperation::Reclaimed {
                previous,
                claim: claim.clone(),
            },
        )
        .await?;
    } else {
        insert_operation(
            tx,
            &DispatchOperation::Claimed {
                claim: claim.clone(),
            },
        )
        .await?;
    }
    let rows = sqlx::query(&format!(
        "SELECT message_id, thread_id, correlation_id, result, context_messages, available_at \
         FROM {p}_pending WHERE run_id = $1 \
         AND (available_at IS NULL OR available_at <= $2) ORDER BY created_at"
    ))
    .bind(&requested_run.0)
    .bind(crate::clock::db_millis(now_ms))
    .fetch_all(&mut **tx)
    .await
    .map_err(store_error)?;
    let mut pending = Vec::with_capacity(rows.len());
    for row in rows {
        let Json(result): Json<ResumeResult> = row.try_get("result").map_err(store_error)?;
        let context_messages = row
            .try_get::<Option<Json<Vec<Message>>>, _>("context_messages")
            .map_err(store_error)?
            .map(|Json(messages)| messages)
            .unwrap_or_default();
        pending.push(PendingInput {
            message_id: row.try_get("message_id").map_err(store_error)?,
            run_id: requested_run.clone(),
            thread_id: ThreadId(row.try_get("thread_id").map_err(store_error)?),
            correlation_id: row.try_get("correlation_id").map_err(store_error)?,
            available_at_ms: row
                .try_get::<Option<i64>, _>("available_at")
                .map_err(store_error)?
                .map(crate::clock::millis_from_db)
                .transpose()
                .map_err(|err| DispatchError::Rejected(err.to_string()))?,
            result,
            context_messages,
        });
    }
    Ok(Some(Claimed {
        session_work_authority: None,
        request,
        lease: Lease {
            run_id: claim.run_id,
            owner: claim.owner,
            expires_ms: expires,
            epoch: claim.epoch,
        },
        credential_bindings,
        credential_deliveries: Vec::new(),
        cancellation_requested: cancellation_requested != 0,
        pending,
        recovered: status == "running",
        session_activity_admission_required: mode == ExactClaimMode::ReservationRecovery,
        sandbox,
        assignment: (!terminal_resolution)
            .then(|| worker.map(WorkerAssignment::from))
            .flatten(),
    }))
}
