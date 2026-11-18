// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Captures and restores the Dispatch state published by the object-backed authority.
//! Versioned materialization of the existing Dispatch transition state.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Persisted scheduling data only. Clock, locks and diagnostic caches are
/// injected when a document is materialized and never become durable state.
#[derive(serde::Serialize, serde::Deserialize)]
struct DispatchSnapshot {
    format: u32,
    order: Vec<RunId>,
    rows: BTreeMap<RunId, Row>,
    pending: Vec<PendingRow>,
    outbox: Vec<PendingInput>,
    completions: Vec<CompletionRecord>,
    operations: Vec<crate::dispatch::StoredDispatchEvent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    claim_requests: BTreeMap<String, ObjectClaimReceipt>,
}
impl MemoryDispatchStore {
    pub(crate) fn from_snapshot(
        snapshot: Option<&str>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, DispatchError> {
        let store = Self::new(clock.clone());
        if let Some(snapshot) = snapshot {
            let snapshot: DispatchSnapshot =
                serde_json::from_str(snapshot).map_err(DispatchError::unavailable)?;
            if snapshot.format != 1 {
                return Err(DispatchError::Rejected(
                    "unsupported Dispatch snapshot".into(),
                ));
            }
            let mut state = lock(&store.state)?;
            state.order = snapshot.order;
            state.rows = snapshot.rows;
            state.pending = snapshot.pending;
            state.outbox = snapshot.outbox;
            state.completions = snapshot.completions;
            state.operations = snapshot.operations;
            state.claim_requests = snapshot.claim_requests;
        }
        Ok(store)
    }
    pub(crate) fn snapshot(&self) -> Result<String, DispatchError> {
        let state = lock(&self.state)?;
        serde_json::to_string(&DispatchSnapshot {
            format: 1,
            order: state.order.clone(),
            rows: state.rows.clone(),
            pending: state.pending.clone(),
            outbox: state.outbox.clone(),
            completions: state.completions.clone(),
            operations: state.operations.clone(),
            claim_requests: state.claim_requests.clone(),
        })
        .map_err(DispatchError::unavailable)
    }
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub(super) struct ObjectClaimReceipt {
    requested_run: Option<RunId>,
    claimed: Claimed,
    #[serde(default)]
    expires_at_ms: u64,
}

impl MemoryDispatchStore {
    /// A conservative lower bound. Exact eligibility (peer runs, cancellation,
    /// Worker eligibility and physical ownership) remains at the claim kernel.
    pub(crate) fn next_claim_at(&self) -> Result<Option<u64>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .rows
            .iter()
            .filter_map(|(id, row)| {
                let due = match row.state {
                    DispatchState::Reserved | DispatchState::ReservationLeased => {
                        row.exact_claim_deadline()?
                    }
                    DispatchState::Leased => row.lease.as_ref()?.expires_ms(),
                    DispatchState::Pending => 0,
                    DispatchState::Awaiting if row.cancellation_requested => 0,
                    DispatchState::Awaiting => state
                        .pending
                        .iter()
                        .filter(|pending| pending.input.run_id == *id)
                        .map(|pending| pending.input.available_at_ms.unwrap_or(0))
                        .min()?,
                    DispatchState::DeadLetter | DispatchState::Superseded => return None,
                };
                // Retry-exhausted claims may have a different due condition;
                // advertising an early date is safe for both claim modes.
                Some(due)
            })
            .min())
    }

    pub(crate) fn requested_claim(
        &self,
        identity: &str,
        request: &awaken_dispatch_contract::ClaimRequest,
    ) -> Result<Option<Option<Claimed>>, DispatchError> {
        let state = lock(&self.state)?;
        let Some(receipt) = state.claim_requests.get(identity) else {
            return Ok(None);
        };
        if request.run_id != receipt.requested_run {
            return Err(DispatchError::Conflict(
                "claim request already targets different work".into(),
            ));
        }
        let claim = RunClaim::from(&receipt.claimed.lease);
        let live = state.rows.get(&claim.run_id).is_some_and(|row| {
            matches!(
                row.state,
                DispatchState::Leased | DispatchState::ReservationLeased
            ) && row.lease_epoch == claim.epoch
                && row.lease.as_ref().is_some_and(|lease| {
                    lease.owner() == claim.owner && lease.expires_ms() > self.clock.now_ms()
                })
        });
        Ok(Some(live.then(|| receipt.claimed.clone())))
    }

    pub(crate) fn record_claim_request(
        &self,
        identity: String,
        request: awaken_dispatch_contract::ClaimRequest,
        claimed: Claimed,
        expires_at_ms: u64,
        cutoff: u64,
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        state
            .claim_requests
            .retain(|_, receipt| receipt.expires_at_ms == 0 || receipt.expires_at_ms > cutoff);
        let old = state.claim_requests.insert(
            identity,
            ObjectClaimReceipt {
                requested_run: request.run_id,
                claimed,
                expires_at_ms,
            },
        );
        debug_assert!(old.is_none());
        Ok(())
    }
}

pub(super) mod state_codec {
    use super::DispatchState;
    pub(crate) fn serialize<S: serde::Serializer>(
        state: &DispatchState,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match state {
            DispatchState::Reserved => "reserved",
            DispatchState::ReservationLeased => "reservation_leased",
            DispatchState::Pending => "pending",
            DispatchState::Leased => "leased",
            DispatchState::Awaiting => "awaiting",
            DispatchState::DeadLetter => "dead_letter",
            DispatchState::Superseded => "superseded",
        })
    }
    pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DispatchState, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        match value.as_str() {
            "reserved" => Ok(DispatchState::Reserved),
            "reservation_leased" => Ok(DispatchState::ReservationLeased),
            "pending" => Ok(DispatchState::Pending),
            "leased" => Ok(DispatchState::Leased),
            "awaiting" => Ok(DispatchState::Awaiting),
            "dead_letter" => Ok(DispatchState::DeadLetter),
            "superseded" => Ok(DispatchState::Superseded),
            _ => Err(serde::de::Error::custom("unknown Dispatch state")),
        }
    }
}

impl MemoryDispatchStore {
    pub(crate) fn accepted_since(&self, before: &str) -> Result<Vec<RunDispatch>, DispatchError> {
        let before: DispatchSnapshot =
            serde_json::from_str(before).map_err(DispatchError::unavailable)?;
        let mut known = before
            .rows
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        known.extend(
            before
                .completions
                .iter()
                .map(|record| record.receipt.run_id.clone()),
        );
        let state = lock(&self.state)?;
        Ok(state
            .order
            .iter()
            .filter(|id| !known.contains(*id))
            .filter_map(|id| state.rows.get(id))
            .map(|row| row.request.clone())
            .collect())
    }
}

impl MemoryDispatchStore {
    pub(crate) fn publication_markers(
        &self,
    ) -> Result<BTreeMap<String, awaken_object_storage::PublicationMarker>, DispatchError> {
        let state = lock(&self.state)?;
        let mut markers = BTreeMap::new();
        for row in state.rows.values() {
            if row.lease.is_some() || row.physical_attempt()?.is_some() {
                markers.insert(
                    format!("history-blocked:{}", row.request.thread_id().0),
                    awaken_object_storage::PublicationMarker {
                        value: "active_writer".into(),
                        expires_ms: u64::MAX,
                    },
                );
            }
        }
        for (id, row) in &state.rows {
            let prefix = match row.state {
                DispatchState::Leased => "commit",
                DispatchState::ReservationLeased => "reservation",
                _ => continue,
            };
            let Some(lease) = &row.lease else { continue };
            let mut authority = row.clone();
            authority.lease = Some(lease.with_deadline(0));
            authority.credential_receipts.clear();
            markers.insert(
                format!(
                    "{prefix}:{}:{}:{}:{}",
                    id.0,
                    lease.owner(),
                    row.lease_epoch,
                    if row.cancellation_requested {
                        "cancelled"
                    } else {
                        "active"
                    }
                ),
                awaken_object_storage::PublicationMarker {
                    value: serde_json::to_string(&authority).map_err(DispatchError::unavailable)?,
                    expires_ms: lease.expires_ms(),
                },
            );
        }
        Ok(markers)
    }
}

impl MemoryDispatchStore {
    pub(crate) fn conditional_claim_authority(
        &self,
        claim: &RunClaim,
        reservation: bool,
        condition: awaken_session_types::SessionPublicationCondition,
    ) -> Result<Option<awaken_dispatch_contract::ConditionalClaimAuthority>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(guarded_claim_row(
            &state,
            claim,
            if reservation {
                DispatchState::ReservationLeased
            } else {
                DispatchState::Leased
            },
        )?
        .map(
            |observation| awaken_dispatch_contract::ConditionalClaimAuthority {
                observation,
                condition,
            },
        ))
    }
}

impl MemoryDispatchStore {
    pub(crate) fn identity_records_since(
        &self,
        before: &str,
        session: &str,
    ) -> Result<Vec<awaken_object_storage::ImmutableSessionRecord>, DispatchError> {
        use awaken_object_storage::ImmutableSessionRecord;
        let accepted = self.accepted_since(before)?;
        let before: DispatchSnapshot =
            serde_json::from_str(before).map_err(DispatchError::unavailable)?;
        let prior_inputs = before
            .pending
            .iter()
            .map(|row| row.input.message_id.as_str())
            .chain(before.outbox.iter().map(|input| input.message_id.as_str()))
            .collect::<std::collections::BTreeSet<_>>();
        let mut records = BTreeMap::new();
        for request in accepted {
            records.insert(
                format!("run:{}", request.run_id().0),
                ImmutableSessionRecord {
                    id: format!("run:{}", request.run_id().0),
                    kind: "run_accepted".into(),
                    payload: serde_json::to_string(&request).map_err(DispatchError::unavailable)?,
                },
            );
            let id = format!("thread:{}", request.thread_id().0);
            records.insert(
                id.clone(),
                ImmutableSessionRecord {
                    id,
                    kind: "thread_binding".into(),
                    payload: serde_json::to_string(
                        &serde_json::json!({"session":session,"thread":request.thread_id().0}),
                    )
                    .map_err(DispatchError::unavailable)?,
                },
            );
        }
        let state = lock(&self.state)?;
        for input in state
            .pending
            .iter()
            .map(|row| &row.input)
            .chain(state.outbox.iter())
        {
            if !prior_inputs.contains(input.message_id.as_str()) {
                let id = format!("input:{}", input.message_id);
                records.insert(
                    id.clone(),
                    ImmutableSessionRecord {
                        id,
                        kind: "input_binding".into(),
                        payload: serde_json::to_string(
                            &serde_json::json!({"session":session,"thread":input.thread_id.0}),
                        )
                        .map_err(DispatchError::unavailable)?,
                    },
                );
                let id = format!("thread:{}", input.thread_id.0);
                records.insert(
                    id.clone(),
                    ImmutableSessionRecord {
                        id,
                        kind: "thread_binding".into(),
                        payload: serde_json::to_string(
                            &serde_json::json!({"session":session,"thread":input.thread_id.0}),
                        )
                        .map_err(DispatchError::unavailable)?,
                    },
                );
            }
        }
        Ok(records.into_values().collect())
    }
}

impl MemoryDispatchStore {
    /// Read-only cross-Session discovery through the same selection kernel.
    /// The resulting transient leases are discarded; only the target Session's
    /// subsequent CAS can issue a durable claim.
    pub(crate) fn discovery_snapshot(
        snapshots: &[awaken_object_storage::SessionObject],
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, DispatchError> {
        let store = Self::new(clock);
        {
            let mut state = lock(&store.state)?;
            for object in snapshots {
                let snapshot: DispatchSnapshot =
                    serde_json::from_str(object.payload.as_deref().ok_or_else(|| {
                        DispatchError::Rejected("discovery component is absent".into())
                    })?)
                    .map_err(DispatchError::unavailable)?;
                if snapshot.format != 1 {
                    return Err(DispatchError::Rejected(
                        "unsupported Dispatch snapshot".into(),
                    ));
                }
                state.order.extend(snapshot.order);
                for (id, row) in snapshot.rows {
                    if state.rows.insert(id, row).is_some() {
                        return Err(DispatchError::Rejected(
                            "Run exists in multiple Session partitions".into(),
                        ));
                    }
                }
                state.pending.extend(snapshot.pending);
            }
        }
        Ok(store)
    }
}
