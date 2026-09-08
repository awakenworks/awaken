impl SqliteDispatchStore {
    async fn read_dispatch_summaries(
        &self,
        run_id: Option<String>,
    ) -> Result<Vec<DispatchSummary>, DispatchError> {
        self.with_conn(move |conn, p| {
            let suffix = if run_id.is_some() { "WHERE run_id = ?1" } else { "ORDER BY created_at" };
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT run_id, thread_id, request, status, attempt_count, cancel_requested, sandbox, lease_until, active_attempt_epoch FROM {p}_dispatch \
                     {suffix}"
                ))
                .map_err(store_error)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(run_id.iter()), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)? != 0,
                        r.get::<_, Option<String>>(6)?.is_some(),
                        r.get::<_, Option<i64>>(7)?,
                        r.get::<_, Option<i64>>(8)?.is_some(),
                    ))
                })
                .map_err(store_error)?;
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
                    physical_attempt_active,
                ) = row.map_err(store_error)?;
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
        })
        .await
    }
}
