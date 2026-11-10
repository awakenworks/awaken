// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: the SQLite dispatch operational reads, including the exact runnable-depth
// @formal reason: query moved from the existing queue trait implementation. Dynamic path: the trait
// @formal reason: delegates to the same store-clock and connection-scoped status, lease, pending-input,
// @formal reason: Thread, and retry predicates; no count cache, lease authority, or second query owner is
// @formal reason: introduced. Evidence: shared dispatch query conformance and process sampler tests; this
// @formal reason: is a product-requirement classification, not a direct formal-proof claim.
type DispatchSummaryRow = (
    String,
    String,
    String,
    String,
    i64,
    i64,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
    bool,
);

impl SqliteDispatchStore {
    async fn query_dispatch_depths(&self) -> Result<Option<DispatchDepths>, DispatchError> {
        let now_ms = self.clock.now_ms();
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let thread_available = thread_available_for_claim(p);
        // Fixed runtime namespace and closed thread/peer predicates only;
        // the current clock value remains a bind parameter.
        let (runnable, leased): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT (SELECT COUNT(*) FROM {p}_dispatch d WHERE (\
             (d.status IN ('reserved', 'reservation_running') \
               AND d.lease_until IS NOT NULL AND d.lease_until <= ?1 AND {}) OR \
             (d.status = 'pending' AND {thread_available}) OR \
             (d.status = 'running' AND d.lease_until <= ?1) OR \
             (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS (\
               SELECT 1 FROM {p}_pending i WHERE i.run_id = d.run_id \
               AND (i.available_at IS NULL OR i.available_at <= ?1)\
             )) AND {thread_available})) \
             AND (d.retry_not_before IS NULL \
                  OR d.retry_not_before <= ?1)), \
             (SELECT COUNT(*) FROM {p}_dispatch d \
              WHERE d.status IN ('reservation_running', 'running') \
                AND d.lease_until > ?1)",
            no_running_peer(p)
        )))
        .bind(crate::clock::db_millis(now_ms))
        .fetch_one(&mut *conn)
        .await
        .map_err(store_error)?;
        Ok(Some(DispatchDepths {
            runnable: durable_u64("runnable dispatch depth", runnable)?,
            leased: durable_u64("leased dispatch depth", leased)?,
        }))
    }

    async fn read_dispatch_summaries(
        &self,
        run_id: Option<String>,
    ) -> Result<Vec<DispatchSummary>, DispatchError> {
        let (_authority, mut conn) = self.with_conn().await?;
        let p = NS;
        let suffix = if run_id.is_some() {
            "WHERE run_id = ?1"
        } else {
            "ORDER BY created_at"
        };
        let sql = format!(
            "SELECT run_id, thread_id, request, status, attempt_count, cancel_requested, sandbox, lease_until, active_attempt_owner, active_attempt_epoch, active_attempt_binding, active_attempt_closing FROM {p}_dispatch \
             {suffix}"
        );
        // suffix is one of two literal selection/order clauses; p is built in.
        // The optional run id remains a bind parameter.
        let mut query = sqlx::query_as::<_, DispatchSummaryRow>(sqlx::AssertSqlSafe(sql));
        if let Some(run_id) = run_id {
            query = query.bind(run_id);
        }
        let rows = query.fetch_all(&mut *conn).await.map_err(store_error)?;
        let mut out = Vec::new();
        for (
            run_id,
            thread_id,
            request,
            status,
            attempt_count,
            cancellation_requested,
            sandbox,
            lease_until,
            active_attempt_owner,
            active_attempt_epoch,
            active_attempt_binding,
            active_attempt_closing,
        ) in rows
        {
            let cancellation_requested = cancellation_requested != 0;
            let sandbox_bound = sandbox.is_some();
            let physical_attempt_active = crate::dispatch::decode_physical_attempt(
                &RunId(run_id.clone()),
                active_attempt_owner,
                active_attempt_epoch
                    .map(crate::stored_claim_epoch)
                    .transpose()?,
                active_attempt_binding,
                active_attempt_closing,
            )?
            .is_some();
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
                physical_attempt_active,
            });
        }
        Ok(out)
    }
}
