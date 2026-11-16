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
const RUNNABLE_SCAN_LIMIT: i64 = 128;
/// The same page size as a length, for counting decoded rows.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the candidate page size is the constant 128"
)]
const RUNNABLE_SCAN_LEN: usize = RUNNABLE_SCAN_LIMIT as usize;

const CLAIM_RECEIPT_TTL_MS: i64 = 15 * 60 * 1_000;
const CLAIM_REQUEST_CLOCK_SKEW_MS: i64 = 60 * 1_000;

#[derive(Clone, Copy)]
struct ClaimReceiptRequest<'a> {
    request: &'a awaken_dispatch_contract::ClaimRequest,
    expires_at_ms: i64,
}

impl<'a> ClaimReceiptRequest<'a> {
    fn new(request: &'a awaken_dispatch_contract::ClaimRequest) -> Result<Self, DispatchError> {
        let invalid =
            || DispatchError::Rejected("claim request id must be a canonical UUIDv7".into());
        let id = uuid::Uuid::parse_str(&request.request_id).map_err(|_| invalid())?;
        if id.get_version_num() != 7 || id.to_string() != request.request_id {
            return Err(invalid());
        }
        let (seconds, nanos) = id.get_timestamp().ok_or_else(invalid)?.to_unix();
        let issued_at_ms =
            i64::try_from(seconds * 1_000 + u64::from(nanos / 1_000_000)).map_err(|_| invalid())?;
        Ok(Self {
            request,
            expires_at_ms: issued_at_ms + CLAIM_RECEIPT_TTL_MS,
        })
    }
}

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
/// An expired lease at that generation was already dead-lettered by
/// `refuse_untrustworthy_expired_leases` before discovery; reservation rows
/// are not pruned and are skipped, observed, by `candidate_is_admissible`.
/// The pruned pending and awaiting rows are observed off this path, by the
/// store's periodic depth read (`observe_pruned_claim_generations`).
fn candidate_epoch_not_exhausted() -> &'static str {
    "d.lease_epoch < 9223372036854775807"
}

/// Restrict each read before the per-Thread and dynamic placement checks.
fn runnable_runs_sql(local: bool) -> sqlx::SqlStr {
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
    // Only closed literal branches and helpers using the built-in namespace
    // compose this query. Clock, page and cursor values are bound below.
    sqlx::SqlSafeStr::into_sql_str(sqlx::AssertSqlSafe(format!(
        "SELECT d.run_id, d.request, d.sandbox, d.worker_assignment, \
                d.status, d.cancel_requested, d.lease_epoch, \
                d.physical_rank, d.state_rank, d.priority, d.created_at, \
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
             AND ($8::jsonb IS NULL OR d.cancel_requested = 1 OR \
                  d.status IN ('reserved', 'reservation_running') OR \
                  COALESCE(d.request #> '{{placement,required_capabilities}}', '[]') <@ $8) \
             AND (d.status IN ('reserved', 'reservation_running') OR {epoch_not_exhausted}) \
             AND (NOT $9 OR NOT d.claim_receipts_retired) \
         ) ranked \
             WHERE ($3::integer IS NULL OR \
                 ranked.physical_rank > $3 OR \
                 (ranked.physical_rank = $3 AND ranked.state_rank > $4) OR \
                 (ranked.physical_rank = $3 AND ranked.state_rank = $4 AND ranked.priority < $5) OR \
                 (ranked.physical_rank = $3 AND ranked.state_rank = $4 AND ranked.priority = $5 AND \
                   (ranked.created_at, ranked.run_id) > ($6::timestamptz, $7::text))) \
             ORDER BY ranked.physical_rank, ranked.state_rank, \
                      ranked.priority DESC, ranked.created_at, ranked.run_id \
             LIMIT $2 \
         ) d ORDER BY d.physical_rank, d.state_rank, d.priority DESC, \
                      d.created_at, d.run_id"
    )))
}

/// Exclusive position in the original discovery order. This is a per-call
/// observation, never a persisted queue or Worker ownership coordinate.
struct CandidatePosition {
    physical_rank: i32,
    state_rank: i32,
    priority: i64,
    created_at: sqlx::types::time::OffsetDateTime,
    run_id: String,
}

impl CandidatePosition {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, DispatchError> {
        Ok(Self {
            physical_rank: row.try_get("physical_rank").map_err(store_error)?,
            state_rank: row.try_get("state_rank").map_err(store_error)?,
            priority: row.try_get("priority").map_err(store_error)?,
            created_at: row.try_get("created_at").map_err(store_error)?,
            run_id: row.try_get("run_id").map_err(store_error)?,
        })
    }
}

fn candidate_is_admissible(
    row: &sqlx::postgres::PgRow,
    audience: CandidateAudience<'_>,
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    now_ms: u64,
    reported: &crate::ReportedClaimGenerations,
) -> Result<bool, DispatchError> {
    let thread_eligible: bool = row.try_get("thread_eligible").map_err(store_error)?;
    let local_eligible: bool = row.try_get("local_eligible").map_err(store_error)?;
    if !thread_eligible || !local_eligible {
        return Ok(false);
    }
    let Json(request): Json<RunDispatch> = row.try_get("request").map_err(store_error)?;
    let worker_manifest = match &audience {
        CandidateAudience::Local => None,
        CandidateAudience::Compatible(worker) => Some(&worker.manifest),
        CandidateAudience::Placed { requester, .. } => Some(&requester.manifest),
    };
    if request.worker_codec_admission(worker_manifest).is_err() {
        return Ok(false);
    }
    let status: String = row.try_get("status").map_err(store_error)?;
    let cancellation_requested: i64 = row.try_get("cancel_requested").map_err(store_error)?;
    let lease_epoch: i64 = row.try_get("lease_epoch").map_err(store_error)?;
    // Only reservation rows reach here at an exhausted generation (the SQL
    // prunes the rest), and any row at a negative one; an exact claim would
    // refuse it in its transaction and abort this whole scan, so it is skipped
    // here instead.
    let run_id: String = row.try_get("run_id").map_err(store_error)?;
    if !crate::sql_claim_generation_admits(reported, &run_id, lease_epoch) {
        return Ok(false);
    }
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

struct RunnableRuns {
    runs: Vec<sqlx::postgres::PgRow>,
    observed_at_ms: u64,
}

enum ScanResult {
    Claimed(Box<Claimed>),
    Continue(CandidatePosition),
    Finished,
}

impl PostgresDispatchStore {
    async fn find_runnable_runs(
        &self,
        audience: CandidateAudience<'_>,
        now_ms: u64,
        after: Option<&CandidatePosition>,
        replays_receipts: bool,
    ) -> Result<RunnableRuns, DispatchError> {
        let offered = audience
            .worker()
            .map(|worker| Json(&worker.manifest.capabilities));
        let runs = sqlx::query(runnable_runs_sql(audience.local()))
            .bind(crate::clock::db_millis(now_ms))
            .bind(RUNNABLE_SCAN_LIMIT)
            .bind(after.map(|p| p.physical_rank))
            .bind(after.map(|p| p.state_rank))
            .bind(after.map(|p| p.priority))
            .bind(after.map(|p| p.created_at))
            .bind(after.map(|p| p.run_id.as_str()))
            .bind(offered)
            .bind(replays_receipts)
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(RunnableRuns {
            runs,
            observed_at_ms: now_ms,
        })
    }

    async fn try_claim_discovered(
        &self,
        runnable: RunnableRuns,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
        audience: CandidateAudience<'_>,
        request: Option<ClaimReceiptRequest<'_>>,
    ) -> Result<ScanResult, DispatchError> {
        let more = runnable.runs.len() == RUNNABLE_SCAN_LEN;
        let mut last = None;
        let mut connection = None;
        for row in runnable.runs {
            let position = CandidatePosition::from_row(&row)?;
            let eligible = candidate_is_admissible(
                &row,
                audience,
                capabilities,
                runnable.observed_at_ms,
                &self.reported,
            )?;
            last = Some(position);
            if !eligible {
                continue;
            }
            // Each losing attempt releases its locks. Keep the checkout until the
            // bounded scan ends and release it before reading the continuation.
            if connection.is_none() {
                connection = Some(self.pool.acquire().await.map_err(store_error)?);
            }
            let mut tx = sqlx::Acquire::begin(&mut **connection.as_mut().expect("claim checkout"))
                .await
                .map_err(store_error)?;
            let claimed = claim_exact_transaction(
                &mut tx,
                ThreadLock::SkipBusy,
                &RunId(last.as_ref().expect("scanned run").run_id.clone()),
                owner,
                lease_ms,
                audience.worker(),
                capabilities,
            )
            .await?;
            if let (Some(request), Some(claimed)) = (request, claimed.as_ref())
                && !insert_claim_request(&mut tx, owner, request, claimed).await?
            {
                // Without a receipt, roll back the tentative claim before
                // reading a possible concurrent winner.
                tx.rollback().await.map_err(store_error)?;
                drop(connection);
                return Ok(self
                    .load_requested_claim(owner, request)
                    .await?
                    .flatten()
                    .map_or(ScanResult::Finished, |claim| {
                        ScanResult::Claimed(Box::new(claim))
                    }));
            }
            tx.commit().await.map_err(store_error)?;
            if let Some(claimed) = claimed {
                return Ok(ScanResult::Claimed(Box::new(claimed)));
            }
        }
        Ok(match last {
            Some(last) if more => ScanResult::Continue(last),
            _ => ScanResult::Finished,
        })
    }

    async fn claim_discovered(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
        audience: CandidateAudience<'_>,
        request: Option<ClaimReceiptRequest<'_>>,
    ) -> Result<Option<Claimed>, DispatchError> {
        let discovery = async {
            // One waiter per connection may scan and try exact claims. A
            // burst above the pool size otherwise reads the same queue front
            // repeatedly before there is a connection available to acquire it.
            let _permit = self
                .discovery
                .acquire()
                .await
                .map_err(DispatchError::unavailable)?;
            refuse_untrustworthy_expired_leases(&self.pool).await?;
            let now_ms = crate::postgres_helpers::postgres_now_ms(&self.pool).await?;
            let mut position = None;
            loop {
                let runnable = self
                    .find_runnable_runs(audience, now_ms, position.as_ref(), request.is_some())
                    .await?;
                match self
                    .try_claim_discovered(
                        runnable,
                        owner,
                        lease_ms,
                        capabilities,
                        audience,
                        request,
                    )
                    .await?
                {
                    ScanResult::Claimed(claimed) => return Ok(Some(*claimed)),
                    ScanResult::Continue(last) => position = Some(last),
                    ScanResult::Finished => return Ok(None),
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), discovery)
            .await
            .map_err(|_| {
                DispatchError::Unavailable("candidate discovery deadline exceeded".into())
            })?
    }

    async fn load_requested_claim(
        &self,
        owner: &str,
        receipt: ClaimReceiptRequest<'_>,
    ) -> Result<Option<Option<Claimed>>, DispatchError> {
        let request = receipt.request;
        let (expired, too_new, exists, retired, target, claimed) = sqlx::query_as::<_, (
            bool, bool, bool, bool, Option<String>, Option<Json<Claimed>>,
        )>(
            "SELECT $3 <= GREATEST(clock.now_ms, retention.expired_through_ms),
                    $3 > clock.now_ms + $4,
                    r.request_id IS NOT NULL, r.claimed IS NULL, r.requested_run,
                    CASE WHEN $3 > GREATEST(clock.now_ms, retention.expired_through_ms)
                         AND d.status IN ('running', 'reservation_running')
                         AND d.lease_until > clock.now_ms THEN r.claimed END
             FROM (SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint AS now_ms) clock
             CROSS JOIN runtime_dispatch_claim_retention retention
             LEFT JOIN runtime_dispatch_claim_request r
               ON r.lease_owner = $1 AND r.request_id = $2
             LEFT JOIN runtime_dispatch d ON d.run_id = r.run_id
               AND d.lease_owner = r.lease_owner AND d.lease_epoch = r.lease_epoch
             WHERE retention.singleton",
        )
        .bind(owner)
        .bind(&request.request_id)
        .bind(receipt.expires_at_ms)
        .bind(CLAIM_RECEIPT_TTL_MS + CLAIM_REQUEST_CLOCK_SKEW_MS)
        .fetch_one(&self.pool)
        .await
        .map_err(store_error)?;
        if expired || (exists && retired) {
            return Ok(Some(None));
        }
        if too_new {
            return Err(DispatchError::Rejected(
                "claim request timestamp is ahead of PostgreSQL".into(),
            ));
        }
        if !exists {
            return Ok(None);
        }
        if target.as_deref() != request.run_id.as_ref().map(|id| id.0.as_str()) {
            return Err(DispatchError::Conflict(
                "claim request already targets different work".into(),
            ));
        }
        Ok(Some(claimed.map(|Json(claimed)| claimed)))
    }

    async fn claim_requested_run(
        &self,
        request: ClaimReceiptRequest<'_>,
        run_id: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let owner = worker.identity.lease_owner();
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let claimed = claim_exact_transaction(
            &mut tx,
            ThreadLock::Wait,
            run_id,
            &owner,
            lease_ms,
            Some(worker),
            &installed_worker_credential_capabilities(worker)?,
        )
        .await?;
        if let Some(claimed) = &claimed
            && !insert_claim_request(&mut tx, &owner, request, claimed).await?
        {
            tx.rollback().await.map_err(store_error)?;
            return Ok(self.load_requested_claim(&owner, request).await?.flatten());
        }
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }
}

async fn insert_claim_request(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    receipt: ClaimReceiptRequest<'_>,
    claimed: &Claimed,
) -> Result<bool, DispatchError> {
    let request = receipt.request;
    // GC advances this durable cutoff before removing keys. A racing INSERT
    // locks and rechecks the cutoff even if the database clock moved backward.
    let result = sqlx::query(
        "WITH retention AS MATERIALIZED (
            SELECT expired_through_ms FROM runtime_dispatch_claim_retention
            WHERE singleton FOR SHARE
         )
         INSERT INTO runtime_dispatch_claim_request
         (lease_owner, request_id, requested_run, run_id, lease_epoch, claimed, expires_at_ms)
         SELECT $1, $2, $3, $4, $5, $6, $7 FROM retention, runtime_dispatch d
         CROSS JOIN (SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint AS now_ms) clock
         WHERE d.run_id = $4 AND NOT d.claim_receipts_retired
           AND $7 > GREATEST(clock.now_ms, retention.expired_through_ms)
           AND $7 <= clock.now_ms + $8
         ON CONFLICT (lease_owner, request_id) DO NOTHING",
    )
    .bind(owner)
    .bind(&request.request_id)
    .bind(request.run_id.as_ref().map(|id| &id.0))
    .bind(&claimed.lease.run_id().0)
    .bind(durable_i64("dispatch lease epoch", claimed.lease.epoch())?)
    .bind(Json(claimed))
    .bind(receipt.expires_at_ms)
    .bind(CLAIM_RECEIPT_TTL_MS + CLAIM_REQUEST_CLOCK_SKEW_MS)
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    Ok(result.rows_affected() == 1)
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
    let thread: Option<String> =
        sqlx::query_scalar("SELECT thread_id FROM runtime_dispatch WHERE run_id = $1")
            .bind(&run_id.0)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_error)?;
    if let Some(thread) = &thread {
        // One Thread-before-row ordering for claim and physical admission.
        // The resolved dispatch relation is the authority namespace. Independent
        // schemas must not contend, while different search_path prefixes that
        // resolve this SAME table must retain one fence. current_schema() alone
        // would split that fence. Hash collisions only over-serialize.
        match mode {
            ThreadLock::Wait => {
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(jsonb_build_array('runtime_dispatch'::regclass::oid, $1::text)::text, 0))")
                    .bind(thread)
                    .execute(&mut **tx)
                    .await
                    .map_err(store_error)?;
            }
            ThreadLock::SkipBusy => {
                let acquired: bool =
                    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended(jsonb_build_array('runtime_dispatch'::regclass::oid, $1::text)::text, 0))")
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
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    let now_ms = crate::postgres_helpers::postgres_now_ms(&mut **tx).await?;
    let phase = sqlx::query("SELECT status, lease_until FROM runtime_dispatch WHERE run_id = $1")
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
    capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
                 AND {no_running_peer})) \
                 AND (d.retry_not_before IS NULL OR d.retry_not_before <= $2) \
                 AND $3::bigint IS NULL"
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
    // eligibility is the closed ExactClaimMode match; p is the built-in
    // namespace. Run id, time and retry limits remain bind parameters.
    let Some(row) = sqlx::query(sqlx::AssertSqlSafe(sql))
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
    if request
        .worker_codec_admission(worker.map(|worker| &worker.manifest))
        .is_err()
    {
        return Ok(None);
    }
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
    sqlx::query(
        "UPDATE runtime_dispatch SET status = $9, lease_owner = $1, lease_until = $2, \
         attempt_count = attempt_count + $3, lease_epoch = $5, worker_assignment = $6, \
         credential_bindings = $7, credential_receipts = $8, retry_not_before = NULL \
         WHERE run_id = $4",
    )
    .bind(owner)
    .bind(crate::clock::db_millis(expires))
    .bind(i64::from(status == "running"))
    .bind(&requested_run.0)
    .bind(crate::minted_claim_epoch_column(claim_epoch)?)
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
            epoch: crate::stored_claim_epoch(previous_epoch)?,
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
    let rows = sqlx::query(
        "SELECT message_id, thread_id, correlation_id, result, context_messages, available_at \
         FROM runtime_pending WHERE run_id = $1 \
         AND (available_at IS NULL OR available_at <= $2) ORDER BY created_at",
    )
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
        lease: DispatchLease::from_persisted(claim.run_id, claim.owner, expires, claim.epoch)?,
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

impl PostgresDispatchStore {
    async fn claim_placed_with_request(
        &self,
        requester: &WorkerSnapshot,
        mut workers: Vec<WorkerSnapshot>,
        policy: std::sync::Arc<dyn PlacementPolicy>,
        lease_ms: u64,
        request: Option<ClaimReceiptRequest<'_>>,
    ) -> Result<Option<Claimed>, DispatchError> {
        let capabilities = installed_worker_credential_capabilities(requester)?;
        // A heartbeat can still report a full pool after all of its Runs
        // settled. Conversely, it misses claims whose preparation is in flight.
        // Rank pull requests against live queue reservations on the store clock.
        // The Worker pool still owns the local limit on physical execution.
        let owners: Vec<String> = workers.iter().map(|w| w.identity.lease_owner()).collect();
        let running: std::collections::HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
            "SELECT lease_owner, count(*) FROM runtime_dispatch \
             WHERE lease_owner = ANY($1) AND status = 'running' \
               AND lease_until > extract(epoch FROM clock_timestamp()) * 1000 \
             GROUP BY lease_owner",
        )
        .bind(&owners)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?
        .into_iter()
        .collect();
        for (worker, owner) in workers.iter_mut().zip(owners) {
            worker.in_flight =
                u32::try_from(running.get(&owner).copied().unwrap_or(0)).unwrap_or(u32::MAX);
        }
        let mut requester = requester.clone();
        requester.in_flight = u32::try_from(
            running
                .get(&requester.identity.lease_owner())
                .copied()
                .unwrap_or(0),
        )
        .unwrap_or(u32::MAX);
        self.claim_discovered(
            &requester.identity.lease_owner(),
            lease_ms,
            &capabilities,
            CandidateAudience::Placed {
                requester: &requester,
                workers: &workers,
                policy: policy.as_ref(),
            },
            request,
        )
        .await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchControlClaims for PostgresDispatchStore {
    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: std::sync::Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.claim_placed_with_request(requester, workers, policy, lease_ms, None)
            .await
    }

    async fn claim_for_request(
        &self,
        request: &awaken_dispatch_contract::ClaimRequest,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: std::sync::Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<awaken_dispatch_contract::ClaimRequestOutcome, DispatchError> {
        use awaken_dispatch_contract::ClaimRequestOutcome::{Claimed, Empty};
        let owner = requester.identity.lease_owner();
        let receipt = ClaimReceiptRequest::new(request)?;
        let outcome = |claimed: Option<crate::Claimed>| {
            claimed.map_or(Empty, |claim| Claimed(Box::new(claim)))
        };
        if let Some(replay) = self.load_requested_claim(&owner, receipt).await? {
            return Ok(outcome(replay));
        }
        let claimed = if let Some(run_id) = &request.run_id {
            self.claim_requested_run(receipt, run_id, requester, lease_ms)
                .await?
        } else {
            self.claim_placed_with_request(requester, workers, policy, lease_ms, Some(receipt))
                .await?
        };
        match claimed {
            Some(claimed) => Ok(outcome(Some(claimed))),
            None => Ok(outcome(
                self.load_requested_claim(&owner, receipt).await?.flatten(),
            )),
        }
    }

    async fn claim_for_terminal_recovery(
        &self,
        requested_run: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let claimed = claim_exact_transaction_with_mode(
            &mut tx,
            ThreadLock::Wait,
            requested_run,
            owner,
            lease_ms,
            None,
            &Default::default(),
            ExactClaimMode::TerminalRecovery,
        )
        .await?;
        tx.commit().await.map_err(store_error)?;
        Ok(claimed)
    }
}
