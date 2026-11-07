// @formal surface exclude=test_support_crate
// @formal owner crates/platform/dispatch/awaken-dispatch/tests/guarded_admission_conformance.rs
// @formal reason: Trait-generic guarded physical-admission decision table (ADR-0081 §7). It is a
// @formal reason: tooling-only test library every Dispatch backend's test target calls; no product
// @formal reason: crate links or invokes it, and it admits nothing at runtime.
//! Trait-generic conformance for guarded physical admission
//! (`DispatchAttemptControl::admit_attempt_guarded`).
//!
//! Every Dispatch backend decides physical admission from one law: the
//! incarnation held by the Registry guard must still hold authority at the
//! store's own clock (`WorkerSnapshot::standing(now).holds_authority()`), and
//! an execution binding, when the transport froze one, must name that
//! incarnation. The unbound transport carries no binding and is judged by the
//! same standing law. A backend passes its store, the Registry it shares with
//! that store in production, its clock and its ordinary claim path.

use std::future::Future;

use awaken_dispatch_contract::dispatch::AttemptExecutionBinding;
use awaken_dispatch_contract::{AttemptAdmission, DispatchAttemptControl, DispatchControlReads};
use awaken_worker_contract::{
    RegistryError, RegistryMutation, RunClaim, WorkerDirectory, WorkerHeartbeat, WorkerIdentity,
    WorkerManifest, WorkerRegistration, WorkerSnapshot, WorkerStanding,
};

const TTL_MS: u64 = 3_600_000;

/// The six standings one incarnation can present to physical admission.
pub const STANDINGS: [WorkerStanding; 6] = [
    WorkerStanding::Starting,
    WorkerStanding::Ready,
    WorkerStanding::Draining,
    WorkerStanding::Quiesced,
    WorkerStanding::Expired,
    WorkerStanding::Dead,
];

/// How the attempt names its process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionBinding {
    /// Unbound transport: no execution binding was frozen.
    Unbound,
    /// The binding names the guarded incarnation.
    Exact,
    /// The binding names another incarnation of the same Worker.
    Foreign,
}

/// A current-protocol Worker that advertises the mandatory dispatch codecs, so
/// an ordinary Run is claimable by it; nothing here is an admission cause.
fn registration(worker: &str) -> WorkerRegistration {
    let mut manifest = WorkerManifest {
        runtime_protocol: awaken_worker_contract::VersionRange::exact(
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        ),
        ..WorkerManifest::default()
    };
    manifest.capabilities.extend([
        awaken_worker_contract::PREPARED_SESSION_DISPATCH_CAPABILITY.to_owned(),
        awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY.to_owned(),
    ]);
    WorkerRegistration {
        worker_id: worker.to_string(),
        incarnation_id: "boot".to_string(),
        manifest,
        create_ledger: None,
    }
}

fn heartbeat(sequence: u64, ready: bool) -> WorkerHeartbeat {
    WorkerHeartbeat {
        sequence,
        ready,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        source_retention: Default::default(),
    }
}

async fn applied(mutation: impl Future<Output = Result<RegistryMutation, RegistryError>>) {
    assert_eq!(mutation.await.unwrap(), RegistryMutation::Applied);
}

/// Move a Ready, claimed incarnation into `standing` through the Registry
/// kernel. `Expired` is the half-open boundary: the Registry lease ends
/// exactly at the store's current time (`expires_at_ms == now`).
async fn enter<N, NF>(
    directory: &dyn WorkerDirectory,
    identity: &WorkerIdentity,
    standing: WorkerStanding,
    store_now: &N,
) where
    N: Fn() -> NF,
    NF: Future<Output = u64>,
{
    match standing {
        WorkerStanding::Starting => {
            let now = store_now().await;
            applied(directory.heartbeat(identity, heartbeat(2, false), now, TTL_MS)).await;
        }
        WorkerStanding::Ready => {}
        WorkerStanding::Draining => {
            let now = store_now().await;
            applied(directory.begin_drain(identity, now + TTL_MS)).await;
        }
        WorkerStanding::Quiesced => {
            let now = store_now().await;
            applied(directory.begin_drain(identity, now + TTL_MS)).await;
            applied(directory.mark_quiesced(identity)).await;
        }
        WorkerStanding::Expired => {
            // A heartbeat stamped one TTL ago renews the lease to end exactly
            // now on the store clock. A manual clock holds that instant; a
            // database clock can only move later, which keeps it expired.
            let now = store_now().await;
            applied(directory.heartbeat(identity, heartbeat(2, true), now - TTL_MS, TTL_MS)).await;
            let record = directory
                .current(&identity.worker_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(record.snapshot.expires_at_ms, now, "Expired boundary row");
        }
        WorkerStanding::Dead => applied(directory.deregister(identity)).await,
    }
}

/// Decision table: standing × binding → `admit_attempt_guarded` outcome and
/// slot effect. Causes: C1 the guarded incarnation's standing on the store
/// clock (six values, Expired at `expires_at_ms == now`); C2 binding absent,
/// exact or foreign. Effects: E1 Applied and the slot names the claim; E2
/// Fenced and no slot; E3 rejected and no slot.
/// Rules: R1 holds_authority(C1) + C2∈{absent, exact} → E1;
/// R2 !holds_authority(C1) + C2∈{absent, exact} → E2;
/// R3 C2 foreign → E3 whatever C1: a guard that names another attempt is an
/// integrity error, decided before time is consulted.
pub async fn assert_guarded_admission_follows_standing<S, N, NF, C, CF>(
    backend: &str,
    store: &S,
    directory: &dyn WorkerDirectory,
    store_now: N,
    claim: C,
) where
    S: DispatchAttemptControl + DispatchControlReads + ?Sized,
    N: Fn() -> NF,
    NF: Future<Output = u64>,
    C: Fn(String, WorkerSnapshot) -> CF,
    CF: Future<Output = RunClaim>,
{
    let mut rows = Vec::new();
    let mut mismatches = Vec::new();
    for binding_kind in [
        AdmissionBinding::Unbound,
        AdmissionBinding::Exact,
        AdmissionBinding::Foreign,
    ] {
        for standing in STANDINGS {
            let tag = format!("{backend}-admission-{standing:?}-{binding_kind:?}").to_lowercase();
            let now = store_now().await;
            let identity = directory
                .register(registration(&tag), now, TTL_MS)
                .await
                .unwrap()
                .snapshot
                .identity;
            applied(directory.heartbeat(&identity, heartbeat(1, true), now, TTL_MS)).await;
            let snapshot = directory
                .current(&identity.worker_id)
                .await
                .unwrap()
                .unwrap()
                .snapshot;
            let run_claim = claim(tag.clone(), snapshot).await;
            enter(directory, &identity, standing, &store_now).await;
            let guard = directory
                .lock_incarnation(&identity)
                .await
                .unwrap()
                .expect("the exact incarnation is still the current record");
            let binding = match binding_kind {
                AdmissionBinding::Unbound => None,
                AdmissionBinding::Exact => Some(identity.clone()),
                AdmissionBinding::Foreign => Some(WorkerIdentity::new(
                    identity.worker_id.clone(),
                    "another-boot",
                    identity.generation + 1,
                )),
            }
            .map(|worker| {
                AttemptExecutionBinding::new(worker, "conformance-provider", format!("{tag}-pid"))
                    .unwrap()
            });
            let outcome = store
                .admit_attempt_guarded(&run_claim, binding.as_ref(), guard, false)
                .await;
            let installed = store
                .load_physical_attempt(&run_claim.run_id)
                .await
                .unwrap()
                .is_some_and(|attempt| attempt.claim == run_claim);
            let observed = match &outcome {
                Ok(admission) => format!("{admission:?}"),
                Err(_) => "Rejected".to_string(),
            };
            let expected = if binding_kind == AdmissionBinding::Foreign {
                "Rejected".to_string()
            } else if !standing.holds_authority() {
                format!("{:?}", AttemptAdmission::Fenced)
            } else {
                format!("{:?}", AttemptAdmission::Applied)
            };
            let expected_installed = expected == format!("{:?}", AttemptAdmission::Applied);
            rows.push(format!(
                "{standing:?} × {binding_kind:?} → {observed} (slot installed: {installed})"
            ));
            if observed != expected || installed != expected_installed {
                mismatches.push(format!(
                    "{backend}: {standing:?} × {binding_kind:?}: expected {expected} \
                     (slot {expected_installed}), observed {observed} (slot {installed}) \
                     {outcome:?}"
                ));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{backend} guarded admission table:\n{}\nmismatches: {mismatches:#?}",
        rows.join("\n")
    );
}
