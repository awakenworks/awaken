// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: Original ledger CAS and fresh capture are fail closed;
// @formal reason: the cause-effect tests cover byte and replay scope, not universal physical closure.
//! Derive future obligations from the original state; never persist a balance.
use super::super::capture::RESULT_BYTES;
use super::{GroupAdmission, MAX_TICKETS, WorkerCreateLedgerState};
use crate::{error::backend, names};
use awaken_container_contract::RuntimeError;
use awaken_container_contract::{
    CreateTicket, CreateTicketRequest, TicketOrigin, TicketResult, TicketStatus,
};
use serde::Serialize;

pub(in crate::create_ledger) struct FutureCapacity {
    pub(in crate::create_ledger) data_growth: usize,
    pub(in crate::create_ledger) outer_growth: usize,
}

use super::super::capture::checked_size_add as add;

fn measure<T: Serialize>(value: &T) -> Result<(usize, usize), RuntimeError> {
    let inner = serde_json::to_string(value).map_err(backend)?;
    let outer = serde_json::to_vec(&inner).map_err(backend)?.len();
    Ok((
        inner.len(),
        outer
            .checked_sub(2)
            .ok_or_else(|| backend("invalid JSON size"))?,
    ))
}

fn maximum_status() -> Result<TicketStatus, RuntimeError> {
    // Derive fixed structure from this same serde codec. Backslashes maximize
    // the outer escaping layer. This transient sizing value is never retained
    // or returned as physical UID evidence.
    let overhead = measure(&TicketResult::Created { uid: String::new() })?.0;
    let payload = RESULT_BYTES
        .checked_sub(overhead)
        .ok_or_else(|| backend("invalid result capture envelope"))?;
    let mut uid = "\\".repeat(payload / 2);
    if payload % 2 != 0 {
        uid.push('x');
    }
    Ok(TicketStatus::Finished(TicketResult::Created { uid }))
}

fn potential_claim(ticket: &CreateTicket) -> bool {
    ticket.request.resource == "persistentvolumeclaims"
        && matches!(
            ticket.status,
            TicketStatus::InFlight | TicketStatus::Finished(TicketResult::Created { .. })
        )
}

pub(in crate::create_ledger) fn future_capacity(
    state: &WorkerCreateLedgerState,
) -> Result<FutureCapacity, RuntimeError> {
    let mut growth = FutureCapacity {
        data_growth: 0,
        outer_growth: 0,
    };
    if state.capture_policy.is_none() {
        return Ok(growth);
    }
    let maximum_status = maximum_status()?;
    let maximum = measure(&maximum_status)?;
    // One coordinate index avoids scanning the full Worker inventory once
    // for every ticket/claim. It is transient and confers no membership.
    let pods: std::collections::BTreeMap<_, _> = state
        .tickets
        .iter()
        .filter(|(_, ticket)| ticket.request.resource == "pods")
        .map(|(key, ticket)| {
            (
                (
                    ticket.request.group.as_str(),
                    ticket.request.namespace.as_str(),
                    ticket.request.name.as_str(),
                ),
                key,
            )
        })
        .collect();
    let mut covered = std::collections::BTreeSet::new();
    let mut missing = Vec::new();
    for claim in state
        .tickets
        .values()
        .filter(|ticket| potential_claim(ticket))
    {
        let name = names::pod_for_continuation_claim(&claim.request.name);
        let key = name.as_deref().and_then(|name| {
            pods.get(&(
                claim.request.group.as_str(),
                claim.request.namespace.as_str(),
                name,
            ))
        });
        if let Some(key) = key {
            covered.insert(*key);
        } else {
            missing.push((claim, name));
        }
    }
    let guards = missing.len();
    for (key, ticket) in &state.tickets {
        let rejected_guard = matches!(
            ticket.status,
            TicketStatus::Finished(TicketResult::RejectedNoEffect { .. })
        ) && (ticket.isolation_pending || covered.contains(key));
        if ticket.status == TicketStatus::InFlight || rejected_guard {
            let current = measure(&ticket.status)?;
            growth.data_growth = add(
                growth.data_growth,
                maximum
                    .0
                    .checked_sub(current.0)
                    .ok_or_else(|| backend("fresh status exceeds its reserved data envelope"))?,
            )?;
            growth.outer_growth = add(
                growth.outer_growth,
                maximum
                    .1
                    .checked_sub(current.1)
                    .ok_or_else(|| backend("fresh status exceeds its reserved outer envelope"))?,
            )?;
        }
        if ticket.isolation_pending {
            // true->false grows one ASCII byte after exact isolation joins.
            growth.data_growth = add(growth.data_growth, 1)?;
            growth.outer_growth = add(growth.outer_growth, 1)?;
        }
    }
    for (claim, name) in missing {
        // Unknown/noncanonical names reserve the full admitted253 raw-byte
        // escaped bound. This computes capacity only, never a guard target.
        let guard = CreateTicket {
            request: CreateTicketRequest {
                group: claim.request.group.clone(),
                resource: "pods".into(),
                namespace: claim.request.namespace.clone(),
                name: name.unwrap_or_else(|| "\0".repeat(253)),
                digest: "a".repeat(64),
            },
            status: maximum_status.clone(),
            origin: TicketOrigin::TerminalGuard,
            isolation_id: "a".repeat(32),
            isolation_pending: false,
        };
        // One JSON map entry and comma. Existing claim ensures the map is
        // nonempty. Key stays64hex regardless of eventual target/digest.
        let entry = std::collections::BTreeMap::from([(guard.request.key(), guard)]);
        let size = measure(&entry)?;
        growth.data_growth = add(
            growth.data_growth,
            size.0
                .checked_sub(1)
                .ok_or_else(|| backend("invalid guard size"))?,
        )?;
        growth.outer_growth = add(
            growth.outer_growth,
            size.1
                .checked_sub(1)
                .ok_or_else(|| backend("invalid guard size"))?,
        )?;
    }
    if add(state.tickets.len(), guards)? > MAX_TICKETS {
        return Err(backend(
            "Create ledger must retain its future terminal guard slots",
        ));
    }
    for admission in state.groups.values() {
        if *admission == GroupAdmission::Open {
            growth.data_growth = add(growth.data_growth, 2)?;
            growth.outer_growth = add(growth.outer_growth, 2)?;
        }
    }
    Ok(growth)
}

#[cfg(test)]
mod tests;
