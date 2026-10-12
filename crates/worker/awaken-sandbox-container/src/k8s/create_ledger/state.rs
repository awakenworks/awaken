// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: The provider-owned request state orders Create admission,
// @formal reason: completion and seal before any Kubernetes effect. Unit and
// @formal reason: live CAS tests cover its selected transitions; the annotation
// @formal reason: does not assert a universal refinement or completed broker.
//! The single durable Kubernetes Create admission state machine.
//!
//! The enclosing ConfigMap is updated with resourceVersion CAS. These pure
//! transitions decide the bytes to write; they never turn an in-flight API
//! request, a retry count, or an object 404 into a completion witness.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::k8s::error::backend;

const VERSION: u8 = 1;
const MAX_TICKETS: usize = 2048;
const MAX_GROUPS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum TicketResult {
    Created {
        uid: String,
    },
    RejectedNoEffect {
        code: u16,
    },
    /// A provider-owned inert object occupies the exact Create name.
    /// The independent close reader must reverify its UID and protection
    /// before this result can satisfy any Session terminal obligation.
    Isolated {
        uid: String,
    },
}

impl TicketResult {
    /// Validate the existing definitive-result shape. This does not establish
    /// original request identity, isolation purpose or physical closure; those
    /// remain with the caller and the ledger's controlled transitions.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        match self {
            Self::Created { uid } | Self::Isolated { uid }
                if !uid.trim().is_empty() && uid.trim() == uid =>
            {
                Ok(())
            }
            Self::RejectedNoEffect { code } if matches!(*code, 400 | 401 | 403 | 409 | 422) => {
                Ok(())
            }
            _ => Err(backend(
                "Kubernetes Create result is not a definitive physical observation",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum TicketStatus {
    InFlight,
    Finished(TicketResult),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTicketRequest {
    pub group: String,
    pub resource: String,
    pub namespace: String,
    pub name: String,
    pub digest: String,
}

impl CreateTicketRequest {
    pub fn new(
        group: impl Into<String>,
        resource: impl Into<String>,
        namespace: impl Into<String>,
        name: impl Into<String>,
        digest: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let request = Self {
            group: group.into(),
            resource: resource.into(),
            namespace: namespace.into(),
            name: name.into(),
            digest: digest.into(),
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        let fields = [
            (self.group.as_str(), 256),
            (self.resource.as_str(), 64),
            (self.namespace.as_str(), 253),
            (self.name.as_str(), 253),
        ];
        if fields
            .into_iter()
            .any(|(value, max)| value.is_empty() || value.len() > max || value.trim() != value)
            || !supported_resource(&self.resource)
            || self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(backend(
                "Kubernetes Create ticket has incomplete or unbounded identity",
            ));
        }
        Ok(())
    }

    pub fn key(&self) -> String {
        let encoded =
            serde_json::to_vec(&(&self.group, &self.resource, &self.namespace, &self.name))
                .expect("validated Create ticket identity is serializable");
        blake3::hash(&encoded).to_hex().to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTicket {
    pub request: CreateTicketRequest,
    pub status: TicketStatus,
    /// Worker requests and terminal name reservations share this one ledger.
    /// Historical tickets default to the original Worker request origin.
    #[serde(default)]
    pub origin: TicketOrigin,
    /// Minted by the provider's first CAS admission, never by the Worker.
    /// Historical tickets without it remain Unknown after process loss.
    #[serde(default)]
    pub isolation_id: String,
    /// CASed before a provider-owned reservation POST. Even if the original
    /// Create records a definitive result, join waits for this POST to settle.
    #[serde(default)]
    pub isolation_pending: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketOrigin {
    #[default]
    Worker,
    TerminalGuard,
}

pub(super) fn valid_isolation_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn supported_resource(value: &str) -> bool {
    matches!(
        value,
        "pods" | "persistentvolumeclaims" | "configmaps" | "secrets" | "jobs" | "daemonsets"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupAdmission {
    Open,
    Sealed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TicketAdmission {
    Forward,
    InFlight,
    Finished(TicketResult),
    Sealed,
}

/// Complete means only that no earlier admitted Create request can still
/// materialize. Participant absence/disposal remains a separate provider fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum RequestJoin {
    Unknown,
    Complete(Vec<CreateTicket>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCreateLedgerState {
    version: u8,
    worker_sealed: bool,
    groups: BTreeMap<String, GroupAdmission>,
    tickets: BTreeMap<String, CreateTicket>,
}

impl Default for WorkerCreateLedgerState {
    fn default() -> Self {
        Self {
            version: VERSION,
            worker_sealed: false,
            groups: BTreeMap::new(),
            tickets: BTreeMap::new(),
        }
    }
}

impl WorkerCreateLedgerState {
    fn worker_group_is_sealed(&self, group: &str) -> bool {
        self.worker_sealed && self.groups.get(group) == Some(&GroupAdmission::Sealed)
    }

    fn group_tickets<'a>(&'a self, group: &'a str) -> impl Iterator<Item = &'a CreateTicket> {
        self.tickets
            .values()
            .filter(move |ticket| ticket.request.group == group)
    }

    pub fn decode(bytes: &str) -> Result<Self, RuntimeError> {
        let state: Self = serde_json::from_str(bytes).map_err(backend)?;
        state.validate()?;
        Ok(state)
    }

    pub fn encode(&self) -> Result<String, RuntimeError> {
        self.validate()?;
        serde_json::to_string(self).map_err(backend)
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.version != VERSION
            || self.groups.len() > MAX_GROUPS
            || self.tickets.len() > MAX_TICKETS
            || self
                .groups
                .keys()
                .any(|key| key.is_empty() || key.len() > 256)
        {
            return Err(backend(
                "Kubernetes Create ledger has invalid version or bounds",
            ));
        }
        for (key, ticket) in &self.tickets {
            ticket.request.validate()?;
            if key != &ticket.request.key() || !self.groups.contains_key(&ticket.request.group) {
                return Err(backend(
                    "Kubernetes Create ledger ticket has no exact group or key",
                ));
            }
            if !ticket.isolation_id.is_empty() && !valid_isolation_id(&ticket.isolation_id) {
                return Err(backend("Kubernetes Create isolation identity is malformed"));
            }
            if ticket.origin == TicketOrigin::TerminalGuard
                && (ticket.request.resource != "pods"
                    || !self.worker_group_is_sealed(&ticket.request.group)
                    || !valid_isolation_id(&ticket.isolation_id)
                    || matches!(
                        ticket.status,
                        TicketStatus::Finished(
                            TicketResult::Created { .. } | TicketResult::RejectedNoEffect { .. }
                        )
                    ))
            {
                return Err(backend(
                    "Kubernetes terminal Pod guard is outside a sealed physical request group",
                ));
            }
            if ticket.isolation_pending
                && (!valid_isolation_id(&ticket.isolation_id)
                    || !self.worker_group_is_sealed(&ticket.request.group)
                    || matches!(
                        ticket.status,
                        TicketStatus::Finished(TicketResult::Isolated { .. })
                    ))
            {
                return Err(backend(
                    "Kubernetes Create isolation is pending outside a sealed ticket",
                ));
            }
            if let TicketStatus::Finished(result) = &ticket.status {
                result.validate()?;
                if matches!(result, TicketResult::Isolated { .. })
                    && !valid_isolation_id(&ticket.isolation_id)
                {
                    return Err(backend(
                        "Kubernetes Create isolation lacks its exact admitted identity",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Called before any external API request. A retry of one in-flight ticket
    /// must never forward the Create a second time.
    pub fn admit(
        &mut self,
        request: &CreateTicketRequest,
        isolation_id: &str,
    ) -> Result<TicketAdmission, RuntimeError> {
        request.validate()?;
        if !valid_isolation_id(isolation_id) {
            return Err(backend(
                "Kubernetes Create admission lacks an isolation identity",
            ));
        }
        if self.worker_sealed || self.groups.get(&request.group) == Some(&GroupAdmission::Sealed) {
            return Ok(TicketAdmission::Sealed);
        }
        let key = request.key();
        if let Some(existing) = self.tickets.get(&key) {
            if existing.request != *request {
                return Err(backend(
                    "Kubernetes Create replay changed immutable request bytes",
                ));
            }
            return Ok(match &existing.status {
                TicketStatus::InFlight => TicketAdmission::InFlight,
                TicketStatus::Finished(result) => TicketAdmission::Finished(result.clone()),
            });
        }
        if self.tickets.len() == MAX_TICKETS
            || (self.groups.len() == MAX_GROUPS && !self.groups.contains_key(&request.group))
        {
            return Err(backend("Kubernetes Create ledger capacity is exhausted"));
        }
        self.groups
            .entry(request.group.clone())
            .or_insert(GroupAdmission::Open);
        self.tickets.insert(
            key,
            CreateTicket {
                request: request.clone(),
                status: TicketStatus::InFlight,
                origin: TicketOrigin::Worker,
                isolation_id: isolation_id.to_owned(),
                isolation_pending: false,
            },
        );
        Ok(TicketAdmission::Forward)
    }

    pub fn finish(
        &mut self,
        request: &CreateTicketRequest,
        result: TicketResult,
    ) -> Result<bool, RuntimeError> {
        request.validate()?;
        result.validate()?;
        let ticket = self
            .tickets
            .get_mut(&request.key())
            .ok_or_else(|| backend("Kubernetes Create result has no prior admitted request"))?;
        if ticket.request != *request {
            return Err(backend(
                "Kubernetes Create result belongs to another exact request",
            ));
        }
        if ticket.origin != TicketOrigin::Worker {
            return Err(backend(
                "provider terminal guard cannot accept a Worker Create result",
            ));
        }
        if matches!(result, TicketResult::Isolated { .. }) {
            return Err(backend(
                "Kubernetes Create isolation requires the sealed physical observation path",
            ));
        }
        match &ticket.status {
            TicketStatus::InFlight => {
                ticket.status = TicketStatus::Finished(result);
                Ok(true)
            }
            TicketStatus::Finished(existing) if existing == &result => Ok(false),
            TicketStatus::Finished(_) => Err(backend(
                "Kubernetes Create result conflicts with durable completion",
            )),
        }
    }

    /// A verified same-name reservation can close a request even when the
    /// original POST was definitively rejected before the reservation CAS.
    /// The caller must obtain the physical observation first; this pure state
    /// transition never infers it from the HTTP status.
    pub fn finish_isolation(
        &mut self,
        request: &CreateTicketRequest,
        result: TicketResult,
    ) -> Result<bool, RuntimeError> {
        request.validate()?;
        result.validate()?;
        if !matches!(
            result,
            TicketResult::Isolated { .. } | TicketResult::Created { .. }
        ) {
            return Err(backend("Kubernetes Create isolation result is invalid"));
        }
        let ticket = self
            .ticket(request)
            .ok_or_else(|| backend("Kubernetes Create isolation has no admitted request"))?;
        if ticket.origin == TicketOrigin::TerminalGuard
            && !matches!(result, TicketResult::Isolated { .. })
        {
            return Err(backend(
                "provider terminal guard cannot claim an original Pod Create result",
            ));
        }
        if !valid_isolation_id(&ticket.isolation_id) || !self.worker_group_is_sealed(&request.group)
        {
            return Err(backend("Kubernetes Create isolation lacks a sealed Worker"));
        }
        let ticket = self.tickets.get_mut(&request.key()).expect("checked above");
        if !ticket.isolation_pending {
            return if ticket.status == TicketStatus::Finished(result) {
                Ok(false)
            } else {
                Err(backend(
                    "Kubernetes Create isolation had no prior reservation admission",
                ))
            };
        }
        match &ticket.status {
            TicketStatus::InFlight => {
                ticket.status = TicketStatus::Finished(result);
                ticket.isolation_pending = false;
                Ok(true)
            }
            TicketStatus::Finished(TicketResult::RejectedNoEffect { .. })
                if matches!(result, TicketResult::Isolated { .. }) =>
            {
                ticket.status = TicketStatus::Finished(result);
                ticket.isolation_pending = false;
                Ok(true)
            }
            TicketStatus::Finished(existing) if existing == &result => {
                ticket.isolation_pending = false;
                Ok(true)
            }
            _ => Err(backend(
                "Kubernetes Create isolation conflicts with durable completion",
            )),
        }
    }

    /// CAS this marker before an inert same-Kind POST. A result from the old POST
    /// cannot close the group while the reservation API call might still
    /// publish. A replacement Coordinator replays the same exact reservation.
    pub fn start_isolation(
        &mut self,
        request: &CreateTicketRequest,
    ) -> Result<(bool, bool), RuntimeError> {
        request.validate()?;
        if !self.worker_group_is_sealed(&request.group) {
            return Err(backend(
                "Kubernetes Create isolation requires a sealed Worker group",
            ));
        }
        let ticket = self
            .tickets
            .get_mut(&request.key())
            .filter(|ticket| ticket.request == *request)
            .ok_or_else(|| backend("Kubernetes Create isolation has no exact ticket"))?;
        if !valid_isolation_id(&ticket.isolation_id) {
            return Err(backend(
                "Kubernetes Create isolation lacks an admitted identity",
            ));
        }
        if ticket.isolation_pending {
            return Ok((true, false));
        }
        if ticket.status == TicketStatus::InFlight {
            ticket.isolation_pending = true;
            return Ok((true, true));
        }
        Ok((false, false))
    }

    /// CAS this transition against ticket admission. A close that wins first
    /// excludes later Create; an admitted ticket that wins first must be joined.
    pub fn seal_group(&mut self, group: &str) -> Result<bool, RuntimeError> {
        if group.is_empty() || group.len() > 256 || group.trim() != group {
            return Err(backend("Kubernetes Create group identity is invalid"));
        }
        if let Some(admission) = self.groups.get_mut(group) {
            let changed = *admission != GroupAdmission::Sealed;
            *admission = GroupAdmission::Sealed;
            return Ok(changed);
        }
        if self.groups.len() == MAX_GROUPS {
            return Err(backend(
                "Kubernetes Create ledger group capacity is exhausted",
            ));
        }
        self.groups.insert(group.to_owned(), GroupAdmission::Sealed);
        Ok(true)
    }

    pub fn seal_worker(&mut self) -> bool {
        let changed = !self.worker_sealed;
        self.worker_sealed = true;
        changed
    }

    /// Extend the already sealed Worker group with one provider-owned Pod name
    /// reservation for an original retained PVC that has no live Pod. A prior
    /// rejected Pod ticket retains its original nonce; an absent ticket gets a
    /// distinct typed origin in the same CAS state. Neither case forwards an
    /// ordinary Worker Create or changes an already-created Pod result.
    pub fn admit_terminal_pod_guard(
        &mut self,
        request: &CreateTicketRequest,
        isolation_id: &str,
    ) -> Result<bool, RuntimeError> {
        request.validate()?;
        if request.resource != "pods"
            || !valid_isolation_id(isolation_id)
            || !self.worker_group_is_sealed(&request.group)
        {
            return Err(backend(
                "terminal Pod guard requires one exact sealed Worker group",
            ));
        }
        let key = request.key();
        if let Some(existing) = self.tickets.get_mut(&key) {
            if existing.request.resource != request.resource
                || existing.request.namespace != request.namespace
                || existing.request.name != request.name
                || !valid_isolation_id(&existing.isolation_id)
                || (existing.origin == TicketOrigin::TerminalGuard && existing.request != *request)
            {
                return Err(backend(
                    "terminal Pod guard conflicts with the admitted Create identity",
                ));
            }
            if matches!(
                existing.status,
                TicketStatus::Finished(TicketResult::RejectedNoEffect { .. })
            ) {
                let changed = !existing.isolation_pending;
                existing.isolation_pending = true;
                return Ok(changed);
            }
            return Ok(false);
        }
        if self.tickets.len() == MAX_TICKETS {
            return Err(backend("Kubernetes Create ledger capacity is exhausted"));
        }
        self.tickets.insert(
            key,
            CreateTicket {
                request: request.clone(),
                status: TicketStatus::InFlight,
                origin: TicketOrigin::TerminalGuard,
                isolation_id: isolation_id.to_owned(),
                isolation_pending: false,
            },
        );
        Ok(true)
    }

    pub fn ticket(&self, request: &CreateTicketRequest) -> Option<&CreateTicket> {
        self.tickets
            .get(&request.key())
            .filter(|ticket| ticket.request == *request)
    }

    /// Snapshot the already sealed group's requests. Only the original
    /// Worker-exit path may use this for physical name isolation; an open group
    /// never gains a second admission or Create executor.
    pub fn sealed_worker_group_tickets(&self, group: &str) -> Option<Vec<CreateTicket>> {
        self.worker_group_is_sealed(group)
            .then(|| self.group_tickets(group).cloned().collect())
    }

    /// Read an already closed Worker group without changing admission. The
    /// stronger Worker prerequisite composes with the sole group join law;
    /// ordinary group-only callers retain their original semantics below.
    pub fn join_sealed_worker_group(&self, group: &str) -> RequestJoin {
        if !self.worker_sealed {
            return RequestJoin::Unknown;
        }
        self.join_group(group)
    }

    pub fn join_group(&self, group: &str) -> RequestJoin {
        if self.groups.get(group) != Some(&GroupAdmission::Sealed) {
            return RequestJoin::Unknown;
        }
        let mut completed = Vec::new();
        for ticket in self.group_tickets(group) {
            if ticket.status == TicketStatus::InFlight || ticket.isolation_pending {
                return RequestJoin::Unknown;
            }
            completed.push(ticket.clone());
        }
        RequestJoin::Complete(completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISOLATION_ID: &str = "0123456789abcdef0123456789abcdef";

    fn request(name: &str, digest: char) -> CreateTicketRequest {
        CreateTicketRequest::new(
            "effect-a",
            "pods",
            "sandbox-ns",
            name,
            digest.to_string().repeat(64),
        )
        .unwrap()
    }

    #[test]
    fn terminal_claim_pod_guard_is_one_replayable_sealed_ticket() {
        // Guard admission table. Causes: C1 exact Worker and group are sealed;
        // C2 an original Pod ticket is absent, definitively rejected, created,
        // or historical without nonce; C3 provider reservation POST settles or
        // loses its response; C4 the Coordinator restarts. Effects: E1 only
        // C1+absent inserts one typed InFlight guard; E2 C1+rejected reuses the
        // original nonce and marks isolation pending; E3 pending keeps join
        // Unknown across C4; E4 one verified Isolated UID joins, while Worker
        // Create admission remains sealed; E5 unsealed, legacy or conflicting
        // results reject. Rules G1 absent=>E1+E3+E4, G2 rejected=>E2+E3+E4,
        // G3 created=>no new guard, G4 !C1/legacy/conflict=>E5.
        let guard = request("pod-a", 'f');
        let mut state = WorkerCreateLedgerState::default();
        assert!(
            state
                .admit_terminal_pod_guard(&guard, ISOLATION_ID)
                .is_err(),
            "G4"
        );
        state.seal_group("effect-a").unwrap();
        state.seal_worker();
        assert!(
            state
                .admit_terminal_pod_guard(&guard, ISOLATION_ID)
                .unwrap(),
            "G1"
        );
        assert!(
            !state
                .admit_terminal_pod_guard(&guard, "f".repeat(32).as_str())
                .unwrap(),
            "G1 replay keeps first nonce"
        );
        assert_eq!(state.join_group("effect-a"), RequestJoin::Unknown, "G1/E3");
        let mut restarted = WorkerCreateLedgerState::decode(&state.encode().unwrap()).unwrap();
        let ticket = restarted.ticket(&guard).unwrap();
        assert_eq!(
            ticket.origin,
            TicketOrigin::TerminalGuard,
            "G1 typed origin"
        );
        assert_eq!(ticket.isolation_id, ISOLATION_ID, "G1 original nonce");
        assert_eq!(
            restarted.start_isolation(&guard).unwrap(),
            (true, true),
            "G1 POST admission"
        );
        assert_eq!(
            restarted.join_group("effect-a"),
            RequestJoin::Unknown,
            "G1/C3+C4"
        );
        assert!(
            restarted
                .finish(&guard, TicketResult::Created { uid: "old".into() })
                .is_err(),
            "G4 provider guard is not an original Create"
        );
        assert!(
            restarted
                .finish_isolation(&guard, TicketResult::Created { uid: "old".into() })
                .is_err(),
            "G4 cannot forge original Pod"
        );
        restarted
            .finish_isolation(
                &guard,
                TicketResult::Isolated {
                    uid: "guard-uid".into(),
                },
            )
            .unwrap();
        assert!(
            matches!(restarted.join_group("effect-a"), RequestJoin::Complete(tickets) if tickets.len() == 1 && tickets[0].origin == TicketOrigin::TerminalGuard),
            "G1/E4"
        );
        assert_eq!(
            restarted.admit(&guard, ISOLATION_ID).unwrap(),
            TicketAdmission::Sealed,
            "G1/E4 no late Worker Create"
        );

        let original = request("pod-b", 'a');
        let mut rejected = WorkerCreateLedgerState::default();
        rejected.admit(&original, ISOLATION_ID).unwrap();
        rejected
            .finish(&original, TicketResult::RejectedNoEffect { code: 403 })
            .unwrap();
        rejected.seal_group("effect-a").unwrap();
        rejected.seal_worker();
        let replacement_digest = request("pod-b", 'b');
        assert!(
            rejected
                .admit_terminal_pod_guard(&replacement_digest, "f".repeat(32).as_str())
                .unwrap(),
            "G2"
        );
        assert_eq!(
            rejected.join_group("effect-a"),
            RequestJoin::Unknown,
            "G2/E3"
        );
        assert_eq!(
            rejected.ticket(&original).unwrap().isolation_id,
            ISOLATION_ID,
            "G2 original nonce"
        );
        assert_eq!(
            rejected.start_isolation(&original).unwrap(),
            (true, false),
            "G2 replay the pending original"
        );
        rejected
            .finish_isolation(
                &original,
                TicketResult::Isolated {
                    uid: "reserved".into(),
                },
            )
            .unwrap();
        assert!(
            matches!(rejected.join_group("effect-a"), RequestJoin::Complete(tickets) if tickets.len() == 1 && tickets[0].origin == TicketOrigin::Worker),
            "G2/E4"
        );

        let mut created = WorkerCreateLedgerState::default();
        created.admit(&original, ISOLATION_ID).unwrap();
        created
            .finish(
                &original,
                TicketResult::Created {
                    uid: "original-uid".into(),
                },
            )
            .unwrap();
        created.seal_group("effect-a").unwrap();
        created.seal_worker();
        assert!(
            !created
                .admit_terminal_pod_guard(&replacement_digest, ISOLATION_ID)
                .unwrap(),
            "G3"
        );
        assert!(
            matches!(created.join_group("effect-a"), RequestJoin::Complete(tickets) if tickets.len() == 1 && tickets[0].status == TicketStatus::Finished(TicketResult::Created { uid: "original-uid".into() })),
            "G3"
        );

        created
            .tickets
            .get_mut(&original.key())
            .unwrap()
            .isolation_id
            .clear();
        assert!(
            created
                .admit_terminal_pod_guard(&replacement_digest, ISOLATION_ID)
                .is_err(),
            "G4 historical missing nonce"
        );
    }

    #[test]
    fn create_admission_and_close_follow_one_cause_effect_table() {
        // Causes: A a request wins admission; C group close wins; F durable
        // final API result exists; R retry uses identical/different bytes.
        // Effects: E1 A inserts one InFlight ticket; E2 C blocks new tickets;
        // E3 admitted InFlight keeps join Unknown across restart; E4 F joins
        // exact result; E5 R never sends a second Create. Rules R1=A=>E1,
        // R2=A+C+!F=>E2+E3, R3=A+C+F=>E4, R4=C+A=>E2,
        // R5=A+R=>E5. Decode/encode simulates the durable CAS payload.
        // Pure-query rules Q1 both seals+all finished -> original complete set;
        // Q2 group-only seal -> Unknown for the stronger Worker query, while
        // the original group-only join above remains Complete. Q3 a missing
        // group -> Unknown without inserting it; Q7 cold/repeated reads retain
        // identical bytes. A known sealed empty group is distinct from missing.
        // Decode, guard/isolation admission and sealed snapshots use the same
        // both-seal predicate; their existing negative cases retain errors.
        // Snapshot and join borrow the same membership/order. Neither shared
        // rule strengthens the ordinary group-only join or creates a group.
        let first = request("pod-a", 'a');
        let second = request("pod-b", 'b');
        let mut ledger = WorkerCreateLedgerState::default();
        assert_eq!(
            ledger.admit(&first, ISOLATION_ID).unwrap(),
            TicketAdmission::Forward,
            "R1"
        );
        assert_eq!(
            ledger.admit(&first, ISOLATION_ID).unwrap(),
            TicketAdmission::InFlight,
            "R5"
        );
        assert!(
            ledger.admit(&request("pod-a", 'b'), ISOLATION_ID).is_err(),
            "R5 changed bytes"
        );
        assert!(ledger.seal_group("effect-a").unwrap(), "R2");
        assert_eq!(
            ledger.admit(&second, ISOLATION_ID).unwrap(),
            TicketAdmission::Sealed,
            "R2"
        );
        let mut restarted = WorkerCreateLedgerState::decode(&ledger.encode().unwrap()).unwrap();
        assert_eq!(
            restarted.join_group("effect-a"),
            RequestJoin::Unknown,
            "R2/E3"
        );
        assert!(
            restarted
                .finish(
                    &first,
                    TicketResult::Created {
                        uid: "pod-uid".into()
                    }
                )
                .unwrap(),
            "R3"
        );
        assert!(
            !restarted
                .finish(
                    &first,
                    TicketResult::Created {
                        uid: "pod-uid".into()
                    }
                )
                .unwrap(),
            "R3 exact result replay"
        );
        assert!(
            restarted
                .finish(
                    &first,
                    TicketResult::Created {
                        uid: "foreign".into()
                    }
                )
                .is_err(),
            "R3 conflicting result"
        );
        assert!(
            matches!(restarted.join_group("effect-a"), RequestJoin::Complete(tickets) if tickets.len() == 1),
            "R3/E4"
        );
        assert_eq!(
            restarted.join_sealed_worker_group("effect-a"),
            RequestJoin::Unknown,
            "Q2 group seal alone is not a Worker seal"
        );
        restarted.seal_worker();
        let before = restarted.encode().unwrap();
        let joined = restarted.join_sealed_worker_group("effect-a");
        assert!(
            matches!(&joined, RequestJoin::Complete(tickets) if tickets.len() == 1),
            "Q1"
        );
        assert_eq!(
            WorkerCreateLedgerState::decode(&before)
                .unwrap()
                .join_sealed_worker_group("effect-a"),
            joined,
            "Q7 cold exact read"
        );
        assert_eq!(restarted.encode().unwrap(), before, "Q7 no mutation");
        assert_eq!(
            restarted.admit(&first, ISOLATION_ID).unwrap(),
            TicketAdmission::Sealed,
            "R2 sealed retry never forwards"
        );

        let mut close_first = WorkerCreateLedgerState::default();
        close_first.seal_group("effect-a").unwrap();
        assert_eq!(
            close_first.admit(&first, ISOLATION_ID).unwrap(),
            TicketAdmission::Sealed,
            "R4"
        );
        assert_eq!(
            close_first.join_group("effect-a"),
            RequestJoin::Complete(Vec::new()),
            "R4 no request admitted before close"
        );
        assert_eq!(
            close_first.join_sealed_worker_group("effect-a"),
            RequestJoin::Unknown,
            "Q2 ordinary group-only behavior remains distinct"
        );
        close_first.seal_worker();
        let before = close_first.encode().unwrap();
        assert_eq!(
            close_first.join_sealed_worker_group("effect-a"),
            RequestJoin::Complete(Vec::new()),
            "Q1 explicit sealed empty group"
        );
        assert_eq!(
            close_first.join_sealed_worker_group("never-observed"),
            RequestJoin::Unknown,
            "Q3 missing group cannot be manufactured"
        );
        assert_eq!(close_first.encode().unwrap(), before, "Q3/Q7 no new group");
    }

    #[test]
    fn uncertain_or_corrupt_results_never_become_a_closed_request_witness() {
        // U1 transport timeout/5xx/429 has no definitive no-effect result;
        // U2 malformed/cross-group ticket or wrong version is rejected on
        // read; U3 whole Worker closure prevents new admissions but permits
        // an independent executor to complete a previously admitted request.
        // U4 shared result law: Created/Isolated with exact nonblank UID ->
        // validate; empty/blank/outer-spaced -> reject. Embedded controls and
        // long opaque UIDs preserve this law's old semantics (not the handle,
        // Worker coordinate or Package text profiles). Validation alone grants
        // no Created/isolation purpose and does not mutate or complete a ticket.
        // U5 the existing five definitive refusal codes validate; timeout,
        // throttling and server errors do not. Broker and ledger consume this
        // same partition; its original U1 transition assertions stay below.
        for code in [400, 401, 403, 409, 422] {
            assert!(
                TicketResult::RejectedNoEffect { code }.validate().is_ok(),
                "U5 {code}"
            );
        }
        let long_uid = "u".repeat(4097);
        for (uid, valid) in [
            ("pod-uid", true),
            ("", false),
            (" \t ", false),
            (" pod-uid ", false),
            ("pod\nuid", true),
            ("pod\0uid", true),
            (long_uid.as_str(), true),
        ] {
            for result in [
                TicketResult::Created { uid: uid.into() },
                TicketResult::Isolated { uid: uid.into() },
            ] {
                assert_eq!(result.validate().is_ok(), valid, "U4 {result:?}");
            }
        }
        let first = request("pod-a", 'a');
        let mut ledger = WorkerCreateLedgerState::default();
        ledger.admit(&first, ISOLATION_ID).unwrap();
        for code in [408, 429, 500, 503] {
            assert!(
                TicketResult::RejectedNoEffect { code }.validate().is_err(),
                "U1 result {code}"
            );
            assert!(
                ledger
                    .finish(&first, TicketResult::RejectedNoEffect { code })
                    .is_err(),
                "U1 {code}"
            );
        }
        ledger.seal_worker();
        assert_eq!(
            ledger.admit(&request("pod-b", 'b'), ISOLATION_ID).unwrap(),
            TicketAdmission::Sealed,
            "U3"
        );
        ledger.seal_group("effect-a").unwrap();
        assert_eq!(ledger.join_group("effect-a"), RequestJoin::Unknown, "U1");
        ledger
            .finish(&first, TicketResult::RejectedNoEffect { code: 403 })
            .unwrap();
        assert!(
            matches!(ledger.join_group("effect-a"), RequestJoin::Complete(tickets) if tickets.len() == 1),
            "U3"
        );

        let mut value: serde_json::Value = serde_json::from_str(&ledger.encode().unwrap()).unwrap();
        value["version"] = 2.into();
        assert!(
            WorkerCreateLedgerState::decode(&value.to_string()).is_err(),
            "U2 version"
        );
        value["version"] = 1.into();
        let ticket = value["tickets"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap();
        ticket["request"]["group"] = "foreign".into();
        assert!(
            WorkerCreateLedgerState::decode(&value.to_string()).is_err(),
            "U2 group"
        );
    }

    #[test]
    fn full_group_capacity_never_prevents_closing_an_existing_group() {
        // C1 maximum retained groups with one still Open; C2 new versus old
        // group. E1 the old group can still seal and reject later Create;
        // E2 a new group cannot grow the ConfigMap beyond its bound.
        let mut ledger = WorkerCreateLedgerState::default();
        for index in 0..MAX_GROUPS {
            ledger
                .groups
                .insert(format!("group-{index}"), GroupAdmission::Open);
        }
        assert!(ledger.seal_group("group-0").unwrap(), "E1");
        assert!(ledger.seal_group("new-group").is_err(), "E2");
        let existing =
            CreateTicketRequest::new("group-1", "pods", "ns", "pod", "a".repeat(64)).unwrap();
        assert_eq!(
            ledger.admit(&existing, ISOLATION_ID).unwrap(),
            TicketAdmission::Forward,
            "C2 existing group can admit its planned object"
        );
        assert!(
            ledger
                .admit(
                    &CreateTicketRequest::new("new-group", "pods", "ns", "pod", "a".repeat(64))
                        .unwrap(),
                    ISOLATION_ID
                )
                .is_err(),
            "E2"
        );
    }

    #[test]
    fn sealed_pod_isolation_joins_only_the_admitted_exact_ticket() {
        // Causes: A a Pod request won CAS with provider nonce; S both the
        // Worker and group are sealed; P a reservation POST may be launched;
        // R original POST is InFlight or has a definitive refusal; I caller
        // has verified the same-name Pod UID. Effects: E1 A+S+P keeps join
        // Unknown across restart and R; E2 A+S+P+I joins one Isolated result;
        // E3 !S, absent/malformed A or changed UID rejects;
        // E4 replayed admission cannot replace A's nonce. Rules P1=A+S+P->E1,
        // P2=P1+I->E2, P3=!A|!S|!I->E3, P4=A+replay->E4.
        // Q4 the pure stronger query also remains Unknown for InFlight and
        // isolationPending; reading never finishes the original/reservation.
        let pod = request("pod-a", 'a');
        let mut state = WorkerCreateLedgerState::default();
        assert_eq!(
            state.admit(&pod, ISOLATION_ID).unwrap(),
            TicketAdmission::Forward
        );
        assert_eq!(
            state
                .admit(&pod, "fedcba9876543210fedcba9876543210")
                .unwrap(),
            TicketAdmission::InFlight,
            "P3"
        );
        assert_eq!(state.ticket(&pod).unwrap().isolation_id, ISOLATION_ID, "P3");
        let isolated = TicketResult::Isolated {
            uid: "sentinel-uid".into(),
        };
        assert!(
            state.finish_isolation(&pod, isolated.clone()).is_err(),
            "P2 open"
        );
        state.seal_worker();
        state.seal_group("effect-a").unwrap();
        assert_eq!(state.start_isolation(&pod).unwrap(), (true, true), "P1");
        assert_eq!(state.join_group("effect-a"), RequestJoin::Unknown, "P1");
        let before = state.encode().unwrap();
        assert_eq!(
            state.join_sealed_worker_group("effect-a"),
            RequestJoin::Unknown,
            "Q4"
        );
        assert_eq!(
            state.encode().unwrap(),
            before,
            "Q4 unchanged pending isolation"
        );
        let mut state = WorkerCreateLedgerState::decode(&state.encode().unwrap()).unwrap();
        assert_eq!(
            state.start_isolation(&pod).unwrap(),
            (true, false),
            "P1 restart replay"
        );
        assert!(
            state.finish_isolation(&pod, isolated.clone()).unwrap(),
            "P1"
        );
        assert!(
            !state.finish_isolation(&pod, isolated.clone()).unwrap(),
            "P1 replay"
        );
        assert!(
            matches!(state.join_group("effect-a"), RequestJoin::Complete(tickets)
            if tickets.len() == 1 && tickets[0].status == TicketStatus::Finished(isolated.clone()))
        );
        assert!(
            state
                .finish_isolation(
                    &pod,
                    TicketResult::Isolated {
                        uid: "foreign".into()
                    }
                )
                .is_err(),
            "P2 UID"
        );

        let mut refused = WorkerCreateLedgerState::default();
        refused.admit(&pod, ISOLATION_ID).unwrap();
        refused.seal_worker();
        refused.seal_group("effect-a").unwrap();
        assert_eq!(refused.start_isolation(&pod).unwrap(), (true, true), "P1");
        refused
            .finish(&pod, TicketResult::RejectedNoEffect { code: 409 })
            .unwrap();
        assert_eq!(
            refused.join_group("effect-a"),
            RequestJoin::Unknown,
            "P1 old 409 cannot join while reservation POST is pending"
        );
        assert_eq!(
            refused.join_sealed_worker_group("effect-a"),
            RequestJoin::Unknown,
            "Q4 finished original still has pending isolation"
        );
        assert!(
            refused
                .finish_isolation(
                    &pod,
                    TicketResult::Created {
                        uid: "foreign".into()
                    }
                )
                .is_err(),
            "P2 a completed refusal cannot become an original Create"
        );
        assert!(
            refused.finish_isolation(&pod, isolated).unwrap(),
            "P2 response-before-isolation CAS"
        );

        let mut already_rejected = WorkerCreateLedgerState::default();
        already_rejected.admit(&pod, ISOLATION_ID).unwrap();
        already_rejected
            .finish(&pod, TicketResult::RejectedNoEffect { code: 409 })
            .unwrap();
        already_rejected.seal_worker();
        already_rejected.seal_group("effect-a").unwrap();
        assert_eq!(
            already_rejected.start_isolation(&pod).unwrap(),
            (false, false),
            "no reservation POST after definitive rejection"
        );

        let mut original_won = WorkerCreateLedgerState::default();
        original_won.admit(&pod, ISOLATION_ID).unwrap();
        original_won.seal_worker();
        original_won.seal_group("effect-a").unwrap();
        assert_eq!(original_won.start_isolation(&pod).unwrap(), (true, true));
        let original = TicketResult::Created {
            uid: "original-uid".into(),
        };
        original_won.finish(&pod, original.clone()).unwrap();
        assert_eq!(
            original_won.join_group("effect-a"),
            RequestJoin::Unknown,
            "P1 original 201 still waits for reservation POST"
        );
        assert!(
            original_won.finish_isolation(&pod, original).unwrap(),
            "P2 reservation 409 joins exact original UID"
        );
        assert!(matches!(
            original_won.join_group("effect-a"),
            RequestJoin::Complete(_)
        ));

        let mut historical: serde_json::Value =
            serde_json::from_str(&state.encode().unwrap()).unwrap();
        historical["tickets"][&pod.key()]["isolation_id"] = "".into();
        assert!(
            WorkerCreateLedgerState::decode(&historical.to_string()).is_err(),
            "P2 historical ticket cannot claim isolation"
        );
    }

    #[test]
    fn every_brokered_kind_uses_the_same_durable_isolation_transition() {
        // Causes: C1 one of the six admitted Kind collections, or an unknown
        // collection; C2 a valid provider nonce or missing historical nonce;
        // C3 original request remains InFlight after Worker/group seal; C4 an
        // exact physical observation returns the reservation UID. Effects:
        // E1 C1+C2+C3 retains pending isolation across encode/decode; E2 C4
        // joins one Isolated result; E3 unknown C1 or missing C2 refuses
        // admission/close and cannot mint a terminal receipt. Decision rows
        // N1=supported+nonce+sealed->E1, N2=N1+observation->E2,
        // N3=unsupported|missing nonce->E3.
        for resource in [
            "pods",
            "persistentvolumeclaims",
            "configmaps",
            "secrets",
            "jobs",
            "daemonsets",
        ] {
            let request = CreateTicketRequest::new(
                "effect-a",
                resource,
                "sandbox-ns",
                "reserved",
                "a".repeat(64),
            )
            .unwrap();
            let mut state = WorkerCreateLedgerState::default();
            assert_eq!(
                state.admit(&request, ISOLATION_ID).unwrap(),
                TicketAdmission::Forward,
                "N1 {resource}"
            );
            state.seal_worker();
            state.seal_group("effect-a").unwrap();
            assert_eq!(
                state.start_isolation(&request).unwrap(),
                (true, true),
                "N1 {resource}"
            );
            let mut restarted = WorkerCreateLedgerState::decode(&state.encode().unwrap()).unwrap();
            assert_eq!(
                restarted.join_group("effect-a"),
                RequestJoin::Unknown,
                "N1 {resource}"
            );
            restarted
                .finish_isolation(
                    &request,
                    TicketResult::Isolated {
                        uid: "reserved-uid".into(),
                    },
                )
                .unwrap();
            assert!(
                matches!(restarted.join_group("effect-a"), RequestJoin::Complete(tickets)
                if tickets.len() == 1 && tickets[0].status == TicketStatus::Finished(TicketResult::Isolated { uid: "reserved-uid".into() })),
                "N2 {resource}"
            );
            assert!(state.admit(&request, "").is_err(), "N3 {resource}");
            let mut historical: serde_json::Value =
                serde_json::from_str(&state.encode().unwrap()).unwrap();
            historical["tickets"][&request.key()]["isolation_pending"] = false.into();
            historical["tickets"][&request.key()]["isolation_id"] = "".into();
            let mut historical = WorkerCreateLedgerState::decode(&historical.to_string()).unwrap();
            assert_eq!(
                historical.join_group("effect-a"),
                RequestJoin::Unknown,
                "N3 historical {resource}"
            );
            assert!(
                historical.start_isolation(&request).is_err(),
                "N3 historical {resource}"
            );
        }
        assert!(
            CreateTicketRequest::new(
                "effect-a",
                "cronjobs",
                "sandbox-ns",
                "reserved",
                "a".repeat(64)
            )
            .is_err(),
            "N3 unknown Kind"
        );
    }
}
