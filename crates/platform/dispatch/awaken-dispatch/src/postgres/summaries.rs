impl PostgresDispatchStore {
    async fn read_dispatch_summaries(
        &self,
        run_id: Option<String>,
    ) -> Result<Vec<DispatchSummary>, DispatchError> {
        let p = NS;
        let suffix = if run_id.is_some() {
            "WHERE run_id = $1"
        } else {
            "ORDER BY created_at"
        };
        let query = format!(
            "SELECT run_id, thread_id, request, status, attempt_count, cancel_requested, sandbox, lease_until, active_attempt_owner, active_attempt_epoch, active_attempt_binding, active_attempt_closing FROM {p}_dispatch \
             {suffix}"
        );
        // suffix is one of two literal selection/order clauses; p is the
        // built-in namespace. The optional run id is a bind parameter.
        let query = sqlx::query(sqlx::AssertSqlSafe(query));
        let query = if let Some(run_id) = run_id {
            query.bind(run_id)
        } else {
            query
        };
        let rows = query.fetch_all(&self.pool).await.map_err(store_error)?;
        rows.into_iter()
            .map(|row| {
                let Json(request): Json<RunDispatch> =
                    row.try_get("request").map_err(store_error)?;
                let run_id = RunId(row.try_get("run_id").map_err(store_error)?);
                let physical_attempt_active = crate::dispatch::decode_physical_attempt(
                    &run_id,
                    row.try_get("active_attempt_owner").map_err(store_error)?,
                    row.try_get::<Option<i64>, _>("active_attempt_epoch")
                        .map_err(store_error)?
                        .map(crate::stored_claim_epoch)
                        .transpose()?,
                    row.try_get("active_attempt_binding").map_err(store_error)?,
                    row.try_get("active_attempt_closing").map_err(store_error)?,
                )?
                .is_some();
                Ok(DispatchSummary {
                    run_id,
                    thread_id: ThreadId(row.try_get("thread_id").map_err(store_error)?),
                    session_thread_id: request.session_thread_id,
                    session_activity_epoch: request.session_activity_epoch,
                    reservation_deadline_ms: if row
                        .try_get::<String, _>("status")
                        .map_err(store_error)?
                        == "reserved"
                    {
                        row.try_get::<Option<i64>, _>("lease_until")
                            .map_err(store_error)?
                            .map(crate::clock::millis_from_db)
                            .transpose()
                            .map_err(|error| DispatchError::Rejected(error.to_string()))?
                    } else {
                        None
                    },
                    state: {
                        let status = row.try_get::<String, _>("status").map_err(store_error)?;
                        DispatchState::from_db(&status).ok_or_else(|| {
                            DispatchError::Rejected(format!(
                                "unknown persisted dispatch state {status}"
                            ))
                        })?
                    },
                    cancellation_requested: row
                        .try_get::<i64, _>("cancel_requested")
                        .map_err(store_error)?
                        != 0,
                    attempt_count: durable_u64(
                        "dispatch attempt count",
                        row.try_get::<i64, _>("attempt_count")
                            .map_err(store_error)?,
                    )?,
                    sandbox_bound: row
                        .try_get::<Option<String>, _>("sandbox")
                        .map_err(store_error)?
                        .is_some(),
                    physical_attempt_active,
                })
            })
            .collect()
    }
}
