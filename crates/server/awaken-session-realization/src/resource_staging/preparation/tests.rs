//! Resource I/O preparation scope: its lease introduction, removal on drop,
//! and fail-closed waits, over the in-memory installed Sessions.
use super::*;
use crate::execution_authority::ExecutionAuthority;
use crate::session_realizations::SessionRealizationSource as _;
use crate::test_support::InMemorySessionRealizations;
use awaken_lease_contract::fence::Epoch;
use awaken_provisioning_contract::CurrentExecutionAuthority;
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_realization_contract::SessionRealizationLeaseReceipt;
use std::{io, time::Duration};

fn lease(epoch: Epoch) -> SessionRealizationLease {
    SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "boot".into(),
        epoch: epoch.get(),
        expires_at_unix_ms: 60_000,
    }
}

fn proof(epoch: Epoch) -> ObservedSessionRealizationLease {
    // Control coordinates start at zero. Local expiry follows the genuine
    // monotonic request observation, independent of the Worker's UTC clock.
    SessionRealizationLeaseReceipt::issued_at(lease(epoch), 0)
        .unwrap()
        .observed(awaken_authority_clock::authority_proof_started().unwrap())
}

fn authority() -> (Arc<InMemorySessionRealizations>, ExecutionAuthority) {
    let source = Arc::new(InMemorySessionRealizations::default());
    // Projection installation installs the Session before any preparation.
    source.install("session").unwrap();
    let authority = ExecutionAuthority::new(source.realizations(), None);
    (source, authority)
}

#[test]
fn resource_scope_retains_success_and_removes_only_its_unpublished_introduced_lease() {
    // Existing/new lease crossed with failure/success. Finish preserves the
    // installed lease; Drop only removes a new unpublished lease. Competing
    // begin cannot replace the original cancellation identity.
    for existing in [false, true] {
        for finished in [false, true] {
            let (source, authority) = authority();
            let sessions = authority.sessions().clone();
            if existing {
                sessions
                    .update_realization("session", |r| {
                        assert!(r.install_lease(proof(Epoch::FIRST)))
                    })
                    .expect("installed Sessions");
            }
            let guard =
                ResourcePreparation::begin(&authority, "session", proof(Epoch::FIRST)).unwrap();
            let token = guard.test_cancellation().clone();
            assert!(
                ResourcePreparation::begin(&authority, "session", proof(Epoch::FIRST)).is_err()
            );
            assert!(
                sessions
                    .read_realization("session", |r| Arc::ptr_eq(
                        r.staging().preparation.as_ref().unwrap(),
                        &token
                    ))
                    .expect("installed Sessions")
                    .unwrap()
            );
            // Neither the concrete registry variable nor an ExecutionAuthority
            // reference is needed once the preparation has begun.
            drop(source);
            drop(authority);
            guard.validate().unwrap();
            if finished {
                sessions
                    .update_realization("session", |r| guard.finish(r))
                    .expect("installed Sessions")
                    .unwrap();
            }
            drop(guard);
            sessions
                .read_realization("session", |r| {
                    assert!(r.staging().preparation.is_none());
                    assert_eq!(
                        r.execution().authority.lease(),
                        (existing || finished).then_some(&lease(Epoch::FIRST))
                    );
                })
                .expect("installed Sessions")
                .unwrap();
        }
    }
}

#[tokio::test]
async fn resource_scope_cancellation_and_slot_replacement_cannot_erase_a_successor() {
    // A pending authority wait must wake on cancellation, closure, generation
    // replacement, token replacement, removal or an ABA-recreated slot. The
    // stale scope cannot finish or clear another preparation, even if its
    // generation bytes equal the predecessor.
    for row in [
        "cancel",
        "close",
        "generation",
        "token",
        "remove",
        "slot_aba",
    ] {
        let (source, authority) = authority();
        let sessions = authority.sessions().clone();
        let guard = ResourcePreparation::begin(&authority, "session", proof(Epoch::FIRST)).unwrap();
        let successor = Arc::new(awaken_runtime_contract::CancellationToken::new());
        {
            let ended = guard.authority_ended();
            tokio::pin!(ended);
            assert!(futures_util::poll!(ended.as_mut()).is_pending());
            match row {
                "cancel" => guard.test_cancellation().cancel(),
                "close" => sessions
                    .update_realization("session", |r| {
                        assert!(r.close_authority(Some(&lease(Epoch::FIRST))));
                    })
                    .expect("installed Sessions"),
                "generation" => sessions
                    .update_realization("session", |r| {
                        assert!(r.install_lease(proof(
                            Epoch::FIRST.next().expect("second fixture epoch")
                        )));
                    })
                    .expect("installed Sessions"),
                "token" => sessions
                    .update_realization("session", |r| {
                        r.staging_mut().preparation = Some(successor.clone());
                        r.execution().changed.notify_waiters();
                    })
                    .expect("installed Sessions"),
                "remove" => {
                    source.remove_session("session").unwrap();
                }
                "slot_aba" => {
                    source.remove_session("session").unwrap();
                    source.install_realization("session", |r| {
                        assert!(r.install_lease(proof(Epoch::FIRST)));
                        r.staging_mut().preparation = Some(successor.clone());
                    });
                }
                _ => unreachable!(),
            }
            tokio::time::timeout(Duration::from_secs(1), ended.as_mut())
                .await
                .unwrap_or_else(|_| panic!("{row}: authority wait did not settle"));
        }
        assert!(guard.validate().is_err(), "{row}");
        assert!(
            sessions
                .modify_realization("session", |r| guard.finish(r))
                .expect("installed Sessions")
                .is_none_or(|result| result.is_err()),
            "{row}"
        );
        drop(guard);
        match row {
            "remove" => assert!(
                !source.contains("session"),
                "Drop cannot recreate a removed slot"
            ),
            "generation" => sessions
                .read_realization("session", |r| {
                    assert_eq!(
                        r.execution().authority.lease(),
                        Some(&lease(Epoch::FIRST.next().expect("second fixture epoch")))
                    );
                    assert!(r.staging().preparation.is_none());
                })
                .expect("installed Sessions")
                .unwrap(),
            "token" | "slot_aba" => sessions
                .read_realization("session", |r| {
                    assert_eq!(r.execution().authority.lease(), Some(&lease(Epoch::FIRST)));
                    assert!(Arc::ptr_eq(
                        r.staging().preparation.as_ref().unwrap(),
                        &successor
                    ));
                    assert!(!successor.is_cancelled());
                })
                .expect("installed Sessions")
                .unwrap(),
            "cancel" | "close" => sessions
                .read_realization("session", |r| {
                    assert!(r.execution().authority.lease().is_none())
                })
                .expect("installed Sessions")
                .unwrap(),
            _ => unreachable!(),
        }
    }
}

struct DeniedWorker;
impl CurrentExecutionAuthority for DeniedWorker {
    fn with_current(&self, _: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
        Err(io::ErrorKind::PermissionDenied.into())
    }
}

#[test]
fn resource_scope_denied_worker_never_installs_or_reopens_a_session() {
    let (source, _) = authority();
    source.remove_session("session").unwrap();
    let mut authority =
        ExecutionAuthority::new(source.realizations(), Some(Arc::new(DeniedWorker)));
    assert!(ResourcePreparation::begin(&authority, "session", proof(Epoch::FIRST)).is_err());
    assert!(
        !source.contains("session"),
        "Worker denies before the canonical slot write"
    );
    authority = ExecutionAuthority::new(source.realizations(), None);
    assert!(
        ResourcePreparation::begin(&authority, "session", proof(Epoch::FIRST)).is_err(),
        "an allowed preparation still never installs the Session"
    );
    assert!(!source.contains("session"));
    source.install("session").unwrap();
    authority
        .sessions()
        .update_realization("session", |r| {
            assert!(r.install_lease(proof(Epoch::FIRST)));
            assert!(r.close_authority(Some(&lease(Epoch::FIRST))));
        })
        .expect("installed Sessions");
    assert!(ResourcePreparation::begin(&authority, "session", proof(Epoch::FIRST)).is_err());
    authority
        .sessions()
        .read_realization("session", |r| {
            assert!(r.execution().authority.is_closed());
            assert!(r.staging().preparation.is_none());
        })
        .expect("installed Sessions")
        .unwrap();
}
