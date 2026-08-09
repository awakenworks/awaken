//! Exact SQLite dispatch claiming.
//!
//! This module owns the one `BEGIN IMMEDIATE` claim transition shared by
//! ordinary execution and committed-terminal repair. Selection, credential
//! admission, epoch advancement, operational events, and pending-input freeze
//! therefore cannot diverge between the two entry points.

use super::*;

pub(super) fn claim_exact_transaction(
    tx: &rusqlite::Transaction<'_>,
    requested_run: &str,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
) -> Result<Option<Claimed>, DispatchError> {
    claim_exact_transaction_with_mode(
        tx,
        requested_run,
        owner,
        lease_ms,
        now_ms,
        worker,
        capabilities,
        ExactClaimMode::Runnable,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn claim_exact_transaction_with_mode(
    tx: &rusqlite::Transaction<'_>,
    requested_run: &str,
    owner: &str,
    lease_ms: u64,
    now_ms: u64,
    worker: Option<&WorkerSnapshot>,
    capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    mode: ExactClaimMode,
) -> Result<Option<Claimed>, DispatchError> {
    let prefix = NS;
    type PickedDispatch = (
        String,
        Option<String>,
        String,
        Option<String>,
        i64,
        Option<String>,
        i64,
    );
    let not_running = format!(
        "NOT EXISTS (SELECT 1 FROM {prefix}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.status = 'running')"
    );
    let eligibility = match mode {
        ExactClaimMode::Runnable => format!(
            "(d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?2) \
             OR (d.status = 'awaiting' AND (d.cancel_requested = 1 OR EXISTS ( \
               SELECT 1 FROM {prefix}_pending pe WHERE pe.run_id = d.run_id \
               AND (pe.available_at IS NULL OR pe.available_at <= ?2))) AND {not_running}) \
             OR (d.status = 'pending' AND {not_running})"
        ),
        ExactClaimMode::TerminalRecovery => {
            format!(
                "(d.status = 'running' AND d.lease_until IS NOT NULL AND d.lease_until < ?2) \
                 OR (d.status = 'awaiting' AND d.lease_owner IS NULL AND d.lease_until IS NULL \
                 AND {not_running})"
            )
        }
    };
    let sql = format!(
        "SELECT d.request, d.sandbox, d.status, d.worker_assignment, d.cancel_requested, \
                d.lease_owner, d.lease_epoch FROM {prefix}_dispatch d \
         WHERE d.run_id = ?1 AND ({eligibility}) LIMIT 1"
    );
    let picked: Option<PickedDispatch> = tx
        .query_row(
            &sql,
            params![requested_run, crate::clock::db_millis(now_ms)],
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
        .map_err(reject)?;
    let Some((
        request_json,
        sandbox,
        status,
        previous_json,
        cancellation_requested,
        previous_owner,
        previous_epoch,
    )) = picked
    else {
        return Ok(None);
    };
    let request: RunDispatch = serde_json::from_str(&request_json).map_err(json_err)?;
    let previous: Option<WorkerAssignment> = previous_json
        .map(|value| serde_json::from_str(&value).map_err(json_err))
        .transpose()?;
    let terminal_recovery = mode == ExactClaimMode::TerminalRecovery;
    if !terminal_recovery
        && cancellation_requested == 0
        && match worker {
            Some(worker) => can_assign(
                worker,
                &request.placement,
                previous.as_ref(),
                sandbox.is_some(),
                now_ms,
            )
            .is_err(),
            None => !can_claim_locally(&request.placement),
        }
    {
        return Ok(None);
    }
    let claim_epoch = (previous_epoch.max(0) as u64)
        .checked_add(1)
        .ok_or_else(|| DispatchError::Rejected("dispatch claim epoch exhausted".to_string()))?;
    let credential_bindings = if terminal_recovery || cancellation_requested != 0 {
        Vec::new()
    } else {
        compile_attempt_credential_bindings(&request, capabilities, claim_epoch, now_ms).map_err(
            |error| {
                DispatchError::Rejected(format!("credential attempt admission failed: {error}"))
            },
        )?
    };
    let expires = crate::clock::deadline_millis(now_ms, lease_ms);
    tx.execute(
        &format!(
            "UPDATE {prefix}_dispatch SET status = 'running', lease_owner = ?1, \
             lease_until = ?2, attempt_count = attempt_count + ?3, \
             lease_epoch = ?5, worker_assignment = ?6, credential_bindings = ?7, \
             credential_receipts = ?8 WHERE run_id = ?4"
        ),
        params![
            owner,
            crate::clock::db_millis(expires),
            i64::from(status == "running"),
            requested_run,
            claim_epoch as i64,
            (!terminal_recovery)
                .then(|| worker.map(WorkerAssignment::from))
                .flatten()
                .map(|value| json(&value))
                .transpose()?,
            json(&credential_bindings)?,
            json(&Vec::<CredentialRealizationReceipt>::new())?
        ],
    )
    .map_err(reject)?;
    let claim = RunClaim {
        run_id: RunId(requested_run.to_string()),
        owner: owner.to_string(),
        epoch: claim_epoch,
    };
    if status == "running" {
        let previous = RunClaim {
            run_id: RunId(requested_run.to_string()),
            owner: previous_owner.ok_or_else(|| {
                DispatchError::Rejected(
                    "expired running dispatch has no persisted lease owner".to_string(),
                )
            })?,
            epoch: previous_epoch.max(0) as u64,
        };
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::LeaseLost {
                claim: previous.clone(),
                reason: LeaseLossReason::Expired,
            },
        )?;
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::Reclaimed {
                previous,
                claim: claim.clone(),
            },
        )?;
    } else {
        insert_operation(
            tx,
            prefix,
            &DispatchOperation::Claimed {
                claim: claim.clone(),
            },
        )?;
    }
    Ok(Some(Claimed {
        request,
        lease: Lease {
            run_id: claim.run_id,
            owner: claim.owner,
            expires_ms: expires,
            epoch: claim.epoch,
        },
        credential_bindings,
        cancellation_requested: cancellation_requested != 0,
        pending: pending_for_run(tx, prefix, requested_run, now_ms)?,
        recovered: status == "running",
        sandbox,
        assignment: (!terminal_recovery)
            .then(|| worker.map(WorkerAssignment::from))
            .flatten(),
    }))
}
