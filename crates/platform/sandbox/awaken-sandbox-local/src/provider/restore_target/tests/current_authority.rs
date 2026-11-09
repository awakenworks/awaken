//! Current restoration authority across the existing lock and byte-store waits.
use super::*;
use crate::provider::workdir_helper_tests::CheckpointPause;
use pc::test_support::{EffectAuthority, FenceTamper as _};
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

#[tokio::test]
async fn restore_revalidates_current_grant_after_target_lock_and_checkpoint_read() {
    // Cause/effect graph: C1 target={absent,exact existing}; C2 awaited
    // dependency={shared target lock,checkpoint get}; C3 grant={revoked,
    // replaced,expired,renewed,cancelled}. These independent causes yield
    // the 2x2x5 decision table below. R1 denied entry performs no target/store
    // effect; R2 after either actual Pending boundary, only same-tenure renewal
    // may succeed. R3 refusal/cancellation retains the exact target binding and
    // prior bytes, with no archive extraction. R4 renewal restores exact bytes;
    // acquisition alone never reads the store. No timing sleep or fresh target
    // identity is used to turn an uncertain result into success.
    for present in [false, true] {
        for wait in ["lock", "get"] {
            for change in ["revoke", "replace", "expire", "renew", "cancel"] {
                let tmp = tempfile::tempdir().unwrap();
                let provider = LocalProvider::new(tmp.path());
                let spec = workdir_spec("restore-current", false);
                let source = provider.create_sandbox(&spec).await.unwrap();
                let path = source.workspace_path().join("checkpoint-file");
                std::fs::write(path, b"checkpoint-content").unwrap();
                let store = CheckpointStore::default();
                let capture = checkpoint_request_for(&spec.scope);
                let checkpoint = source.create_checkpoint(&capture, &store).await.unwrap();
                source.dispose_unattached().await.unwrap();
                let request =
                    exact_restore_request(&spec, &capture, &checkpoint, "current-restore");
                let evidence = request.evidence(&spec);
                let root = restoration_root(tmp.path(), &evidence).unwrap();
                let fence = pc::SandboxEffectFence::new(
                    &request.effect_id,
                    "worker",
                    "runtime",
                    1,
                    u64::MAX - 1,
                )
                .unwrap();
                let authority = EffectAuthority::new(&fence);
                let effect = pc::SandboxEffect::Fenced {
                    fence: &fence,
                    authority: &authority,
                };
                let denied = EffectAuthority::denied();
                assert!(
                    provider
                        .acquire_restore_sandbox(
                            &spec,
                            &request,
                            pc::SandboxEffect::Fenced {
                                fence: &fence,
                                authority: &denied
                            }
                        )
                        .await
                        .is_err(),
                    "R1"
                );
                assert!(!root.exists());
                assert!(read_binding(&root).unwrap().is_none());
                assert_eq!(store.gets.load(Ordering::Acquire), 0);
                if present {
                    provider
                        .acquire_restore_sandbox(&spec, &request, effect)
                        .await
                        .unwrap();
                    std::fs::write(root.join("retained"), b"preserve").unwrap();
                }
                let lock = if wait == "lock" {
                    Some(lock_target(&root).await.unwrap())
                } else {
                    None
                };
                let gate = Arc::new(tokio::sync::Notify::new());
                if wait == "get" {
                    *store.pause.lock().unwrap() = Some((CheckpointPause::Get, gate.clone()));
                }
                let pending = async {
                    if wait == "lock" {
                        provider
                            .acquire_restore_sandbox(&spec, &request, effect)
                            .await
                            .map(|_| ())
                    } else {
                        provider
                            .restore_exact_sandbox(&spec, &request, &store, effect)
                            .await
                            .map(|_| ())
                    }
                };
                let mut pending = Box::pin(pending);
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                let before_binding = read_binding(&root).unwrap().map(|b| b.evidence);
                let before_exists = root.exists();
                let mut current = fence.clone();
                match change {
                    "revoke" => authority.set(None),
                    "replace" => {
                        current.reassign();
                        authority.set(Some((current, Duration::from_secs(1))));
                    }
                    "expire" => authority.set(Some((current, Duration::ZERO))),
                    "renew" => {
                        current.renew();
                        authority.set(Some((current, Duration::from_secs(1))));
                    }
                    _ => {}
                }
                drop(lock);
                gate.notify_one();
                if change == "cancel" {
                    drop(pending);
                } else {
                    assert_eq!(
                        pending.await.is_ok(),
                        change == "renew",
                        "R2 {present}/{wait}/{change}"
                    );
                }
                if change != "renew" {
                    assert_eq!(root.exists(), before_exists, "R3");
                    assert_eq!(
                        read_binding(&root).unwrap().map(|b| b.evidence),
                        before_binding,
                        "R3"
                    );
                    assert!(!root.join("checkpoint-file").exists(), "R3 no extraction");
                } else if wait == "get" {
                    assert_eq!(
                        std::fs::read(root.join("checkpoint-file")).unwrap(),
                        b"checkpoint-content",
                        "R4"
                    );
                }
                if present {
                    assert_eq!(
                        std::fs::read(root.join("retained")).unwrap(),
                        b"preserve",
                        "R3 original bytes"
                    );
                }
                assert_eq!(
                    store.gets.load(Ordering::Acquire),
                    usize::from(wait == "get"),
                    "R4"
                );
            }
        }
    }
}
