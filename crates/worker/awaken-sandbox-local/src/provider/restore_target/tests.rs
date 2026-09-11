//! Exact restoration acquisition, materialization and terminal disposal share one test owner.
#[tokio::test]
async fn terminal_restored_target_revalidates_after_lock_and_rejects_unfenced_disposal() {
    use pc::test_support::EffectAuthority;
    use std::time::Duration;
    // Cause/effect graph: C1 exact target is bound/absent; C2 the original
    // target lock is held by another participant; C3 authority is revoked,
    // replaced, expired, renewed, or the caller cancels while actually Pending.
    // R1 denied entry has zero target mutation. R2 every C3 except renewal
    // preserves original binding/bytes and returns no success; cancellation
    // releases only the waiter's lock descriptor. R3 renewal deletes exactly.
    // R4 a separately supplied fresh fixture grant retries the SAME target;
    // R5 absent replay still requires current authority. R6 ordinary dispose
    // cannot bypass the request/authority port even with a restored handle.
    // Full 2x5 table; non-expiring UTC isolates the current-authority oracle.
    // Restoring fixture authority below is not production revival of an expired
    // Worker incarnation. Real lock and file I/O are used, with no timing sleeps.
    for present in [false, true] {
        for change in ["revoke", "replace", "expire", "renew", "cancel"] {
            let tmp = tempfile::tempdir().unwrap();
            let provider = LocalProvider::new(tmp.path());
            let spec = workdir_spec("restored-current-authority", false);
            let source = provider.create_sandbox(&spec).await.unwrap();
            let store = CheckpointStore::default();
            let checkpoint_request = checkpoint_request_for(&spec.scope);
            let checkpoint = source
                .checkpoint(&checkpoint_request, &store)
                .await
                .unwrap();
            source.dispose_unattached().await.unwrap();
            let request = exact_restore_request(&spec, &checkpoint_request, &checkpoint, "current");
            let evidence = request.evidence(&spec);
            let root = restoration_root(tmp.path(), &evidence).unwrap();
            if present {
                let target = pc::SandboxProvider::acquire_restore(&provider, &spec, &request)
                    .await
                    .unwrap();
                std::fs::write(root.join("retained"), b"original").unwrap();
                assert!(
                    target.target().dispose_unattached().await.is_err(),
                    "R6 no alternate disposal"
                );
            }
            let binding = read_binding(&root).unwrap().map(|binding| binding.evidence);
            let authorization = restored_disposal_authorization();
            let authority = EffectAuthority::new(authorization.effect_fence());
            assert!(
                pc::SandboxProvider::dispose_restored(
                    &provider,
                    &spec,
                    &request,
                    &authorization,
                    &EffectAuthority::denied()
                )
                .await
                .is_err(),
                "R1"
            );
            assert_eq!(
                read_binding(&root).unwrap().map(|binding| binding.evidence),
                binding
            );
            let held = lock_target(&root).await.unwrap();
            let mut pending = Box::pin(pc::SandboxProvider::dispose_restored(
                &provider,
                &spec,
                &request,
                &authorization,
                &authority,
            ));
            std::future::poll_fn(|cx| {
                assert!(
                    std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                    "{present}/{change}"
                );
                std::task::Poll::Ready(())
            })
            .await;
            {
                let mut current = authority.current.lock().unwrap();
                match change {
                    "revoke" => *current = None,
                    "replace" => current.as_mut().unwrap().0.epoch += 1,
                    "expire" => current.as_mut().unwrap().1 = Duration::ZERO,
                    "renew" => current.as_mut().unwrap().0.expires_at_unix_ms += 1,
                    _ => {}
                }
            }
            drop(held);
            if change == "cancel" {
                drop(pending);
            } else {
                assert_eq!(
                    pending.await.is_ok(),
                    change == "renew",
                    "R2/R3 {present}/{change}"
                );
            }
            if change != "renew" {
                assert_eq!(
                    read_binding(&root).unwrap().map(|binding| binding.evidence),
                    binding,
                    "R2 original binding"
                );
                assert_eq!(root.exists(), present);
                if present {
                    assert_eq!(std::fs::read(root.join("retained")).unwrap(), b"original");
                }
                pc::SandboxProvider::dispose_restored(
                    &provider,
                    &spec,
                    &request,
                    &authorization,
                    &EffectAuthority::new(authorization.effect_fence()),
                )
                .await
                .unwrap();
            }
            assert!(!root.exists(), "R3/R4 exact target gone");
            assert!(read_binding(&root).unwrap().is_none());
            assert!(
                sidecar(&root, "lock").unwrap().is_file(),
                "one stable lock inode survives cleanup"
            );
            assert!(
                pc::SandboxProvider::dispose_restored(
                    &provider,
                    &spec,
                    &request,
                    &authorization,
                    &EffectAuthority::denied()
                )
                .await
                .is_err(),
                "R5 no unauthorized absent success"
            );
            pc::SandboxProvider::dispose_restored(
                &provider,
                &spec,
                &request,
                &authorization,
                &EffectAuthority::new(authorization.effect_fence()),
            )
            .await
            .unwrap();
        }
    }
}
use super::*;
use crate::provider::restore_target;
use crate::provider::workdir_helper_tests::{
    CheckpointStore, checkpoint_request_for, workdir_spec,
};
use pc::Sandbox as _;

fn exact_restore_request(
    spec: &pc::SandboxSpec,
    checkpoint_request: &pc::SandboxCheckpointRequest,
    checkpoint: &pc::SandboxCheckpointRef,
    operation: &str,
) -> pc::SandboxRestoreRequest {
    pc::SandboxRestoreRequest {
        workspace_id: checkpoint_request.workspace_id.clone(),
        session_id: spec.scope.clone(),
        effect_id: format!(
            "blake3:{}",
            crate::content_fingerprint(operation.as_bytes())
        ),
        generation_id: checkpoint_request.generation_id.clone(),
        checkpoint: checkpoint.clone(),
    }
}

fn restored_disposal_authorization() -> pc::SandboxDisposalAuthorization {
    // RD: all restore-target cleanup rules supply the existing caller-owned
    // preparation/successor authorization; the provider must not synthesize it.
    crate::test_disposal_authorization(
        &pc::SandboxEffectFence::new(
            "restore-terminal",
            "terminal-owner",
            "terminal-runtime",
            2,
            u64::MAX - 1,
        )
        .unwrap(),
    )
}

// Exact-restore fidelity table: C1=mutable nested file plus file/directory
// modes; C2=checkpoint bytes are durable; C3=source disposal completed through
// its existing owner; C4=the aggregate projects one exact restore request.
// R1 C1+C2+C3+C4 => the completed result carries request-bound evidence and its
// distinct provider target contains identical bytes and permission metadata.
// R2 exact target disposal, including response-loss replay, removes only that
// target. Directory modes are restored children-first so read-only parents
// cannot block extraction.
#[tokio::test]
async fn checkpoint_dispose_restore_preserves_mutable_filesystem() {
    let restored_disposal = restored_disposal_authorization();
    let authority = pc::test_support::EffectAuthority::new(restored_disposal.effect_fence());

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("checkpoint-session", false);
    let sandbox = provider.create_sandbox(&spec).await.unwrap();
    let file = sandbox.workspace_path().join("workspace/bin/tool");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"mutable state").unwrap();
    // R3 all platforms prove exact bytes and disposal. POSIX mode metadata is
    // an additional Unix effect, not a prerequisite that removes Windows R1/R2.
    #[cfg(unix)]
    {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::set_permissions(
            file.parent().unwrap(),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
    }
    let store = CheckpointStore::default();
    let checkpoint_request = checkpoint_request_for(&spec.scope);
    let receipt = sandbox
        .checkpoint(&checkpoint_request, &store)
        .await
        .unwrap();
    sandbox.dispose_unattached().await.unwrap();
    assert!(!sandbox.workspace_path().exists(), "source terminated");

    let request = exact_restore_request(&spec, &checkpoint_request, &receipt, "fidelity");
    let restored = pc::SandboxProvider::restore(&provider, &spec, &request, &store)
        .await
        .unwrap();
    let expected_evidence = request.evidence(&spec);
    assert_eq!(restored.evidence(), &expected_evidence, "R1 exact evidence");
    request
        .verify_handle(&spec, &pc::Sandbox::handle(restored.target().as_ref()))
        .expect("R1 completed target carries the exact durable evidence");
    let restored_root = restore_target::restoration_root(tmp.path(), &expected_evidence).unwrap();
    let restored_file = restored_root.join("workspace/bin/tool");
    assert_eq!(
        std::fs::read(&restored_file).unwrap(),
        b"mutable state",
        "R1 bytes"
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&restored_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(restored_root.join("workspace/bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o500,
        "directory mode is applied after child extraction"
    );
    drop(restored);
    pc::SandboxProvider::dispose_restored(
        &provider,
        &spec,
        &request,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("R2 exact disposal");
    pc::SandboxProvider::dispose_restored(
        &provider,
        &spec,
        &request,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("R2 disposal response-loss replay");
    assert!(!restored_root.exists(), "R2 only the exact target is gone");
}

#[tokio::test]
async fn exact_restore_target_recovery_and_disposal_are_request_bound() {
    let restored_disposal = restored_disposal_authorization();
    let authority = pc::test_support::EffectAuthority::new(restored_disposal.effect_fence());

    // Exact-target cause/effect table. C1 the source reaches durable disposal
    // preparation P before physical Disposal; C2 the exact target is absent or
    // already bound; C3 the provider process wrapper is original/fresh; C4 a
    // disposal request is exact/foreign; C5 request admission is valid/drifted.
    // T1 C1+C2(absent)+C5(valid) => Created without reading checkpoint bytes.
    // T2 C2(bound)+C3(fresh) => Recovered with the identical handle and no byte
    // read. T3 C4(foreign) leaves the exact target intact; T4 C5(drifted) fails
    // before I/O; T5 C4(exact) removes only that target and absent replay
    // succeeds. This replaces the removed marker/fence restore path rather than
    // preserving it as a compatibility track.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("removed-checkpoint-restore", false);
    let create = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let suspend = pc::SandboxEffectFence::new("suspend", "owner", "runtime", 2, u64::MAX).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("hibernate-terminal", "owner", "runtime", 3, u64::MAX).unwrap();
    let source = provider
        .create_sandbox_for_effect(
            &spec,
            &create,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
        )
        .await
        .unwrap();
    std::fs::write(source.workspace_path().join("value"), b"checkpoint").unwrap();
    let source_handle = pc::Sandbox::handle(&source);
    let store = CheckpointStore::default();
    let checkpoint_request = checkpoint_request_for(&spec.scope);
    let checkpoint = source
        .checkpoint_for_effect(
            &checkpoint_request,
            &store,
            &suspend,
            &pc::test_support::EffectAuthority::new(&suspend),
        )
        .await
        .expect("T1 checkpoint participant");
    drop(source);
    let terminal_sandbox = provider
        .prepare_terminal_sandbox_for_effect(
            &spec,
            Some(&source_handle),
            None,
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("T1 terminal prepare")
        .expect("T1 exact terminal participant");
    terminal_sandbox
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("T1 durable P precedes physical Disposal");
    terminal_sandbox
        .dispose_for_effect(
            &crate::test_disposal_authorization(&terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                crate::test_disposal_authorization(&terminal).effect_fence(),
            ),
        )
        .await
        .expect("T1 physical Disposal");
    assert!(
        !crate::sandbox_dir(tmp.path(), "removed-checkpoint-restore").exists(),
        "T1 disposed source remains absent"
    );
    let request = exact_restore_request(&spec, &checkpoint_request, &checkpoint, "recovery");
    let evidence = request.evidence(&spec);
    let exact_root = restore_target::restoration_root(tmp.path(), &evidence).unwrap();
    let created = pc::SandboxProvider::acquire_restore(&provider, &spec, &request)
        .await
        .expect("T1 exact target creation");
    assert_eq!(
        created.disposition(),
        pc::SandboxRestoreTargetDisposition::Created,
        "T1"
    );
    let created_handle = pc::Sandbox::handle(created.target().as_ref());
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "T1 acquisition never reads checkpoint bytes"
    );
    drop(created);

    let fresh_provider = LocalProvider::new(tmp.path());
    let recovered = pc::SandboxProvider::acquire_restore(&fresh_provider, &spec, &request)
        .await
        .expect("T2 fresh provider recovery");
    assert_eq!(
        recovered.disposition(),
        pc::SandboxRestoreTargetDisposition::Recovered,
        "T2"
    );
    assert_eq!(
        pc::Sandbox::handle(recovered.target().as_ref()),
        created_handle,
        "T2 identical exact handle"
    );
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "T2 recovery remains byte-free"
    );

    let foreign = exact_restore_request(&spec, &checkpoint_request, &checkpoint, "foreign");
    pc::SandboxProvider::dispose_restored(
        &fresh_provider,
        &spec,
        &foreign,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("T3 foreign target is provably absent");
    assert!(exact_root.is_dir(), "T3 exact target remains");

    let mut drifted = request.clone();
    drifted.session_id = "different-session".into();
    assert!(
        pc::SandboxProvider::acquire_restore(&fresh_provider, &spec, &drifted)
            .await
            .is_err(),
        "T4 session drift fails admission"
    );
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "T4 zero store I/O"
    );
    assert!(exact_root.is_dir(), "T4 exact target remains");

    drop(recovered);
    pc::SandboxProvider::dispose_restored(
        &fresh_provider,
        &spec,
        &request,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("T5 exact disposal");
    pc::SandboxProvider::dispose_restored(
        &fresh_provider,
        &spec,
        &request,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("T5 disposal response-loss replay");
    assert!(!exact_root.exists(), "T5 exact target removed");
}

#[tokio::test]
async fn exact_restore_materialization_response_loss_is_idempotent() {
    let restored_disposal = restored_disposal_authorization();
    let authority = pc::test_support::EffectAuthority::new(restored_disposal.effect_fence());

    // Restore-materialization table. C1 the exact target is absent/recovered;
    // C2 the completed provider response is delivered/lost before aggregate
    // publication; C3 request admission is exact/malformed. M1 exact first
    // execution materializes checkpoint bytes and returns request-bound
    // evidence. M2 response-loss replay recovers the same unpublished target,
    // revalidates/re-reads the durable object, and converges its checkpoint
    // entries to the same bytes and handle. M3 malformed identity rejects before
    // target/store I/O and preserves M1/M2. No post-publication Agent mutation is
    // permitted in the M2 window, so this test deliberately models only an
    // unpublished crash residue.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("fenced-restore", false);
    let source = provider.create_sandbox(&spec).await.unwrap();
    std::fs::write(source.workspace_path().join("value"), b"checkpoint").unwrap();
    let store = CheckpointStore::default();
    let checkpoint_request = checkpoint_request_for(&spec.scope);
    let receipt = source
        .checkpoint(&checkpoint_request, &store)
        .await
        .unwrap();
    source.dispose_unattached().await.unwrap();
    let request = exact_restore_request(&spec, &checkpoint_request, &receipt, "response-loss");
    let expected_evidence = request.evidence(&spec);
    let restored_root = restore_target::restoration_root(tmp.path(), &expected_evidence).unwrap();
    let restored = pc::SandboxProvider::restore(&provider, &spec, &request, &store)
        .await
        .expect("M1");
    assert_eq!(restored.evidence(), &expected_evidence, "M1 exact evidence");
    let first_handle = pc::Sandbox::handle(restored.target().as_ref());
    let gets_after_first = store.gets.load(std::sync::atomic::Ordering::Relaxed);
    drop(restored);

    std::fs::write(restored_root.join("value"), b"unpublished-crash-residue").unwrap();
    let replay = pc::SandboxProvider::restore(&provider, &spec, &request, &store)
        .await
        .expect("M2");
    assert_eq!(
        pc::Sandbox::handle(replay.target().as_ref()),
        first_handle,
        "M2 identical exact handle"
    );
    assert_eq!(replay.evidence(), &expected_evidence, "M2 exact evidence");
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        gets_after_first + 1,
        "M2 revalidates durable bytes before completion"
    );
    assert_eq!(
        std::fs::read(restored_root.join("value")).unwrap(),
        b"checkpoint",
        "M2 converges partial unpublished bytes"
    );

    let mut malformed = request.clone();
    malformed.effect_id = "blake3:00".into();
    let gets_before_reject = store.gets.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        pc::SandboxProvider::restore(&provider, &spec, &malformed, &store)
            .await
            .is_err(),
        "M3 malformed identity"
    );
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        gets_before_reject,
        "M3 zero store I/O"
    );
    assert_eq!(
        std::fs::read(restored_root.join("value")).unwrap(),
        b"checkpoint",
        "M3 zero target write"
    );
    drop(replay);
    pc::SandboxProvider::dispose_restored(
        &provider,
        &spec,
        &request,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("M1/M2 exact cleanup");
    pc::SandboxProvider::dispose_restored(
        &provider,
        &spec,
        &request,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("M1/M2 cleanup response-loss replay");
}

// Corruption/recovery table. C1 the exact target is absent/recovered; C2 object
// bytes match/differ from the committed size or digest. F1 C2(different)
// reserves at most the exact provider target but returns no completed Sandbox
// and performs no checkpoint entry writes. F2 fixing the same durable object
// then replaying the same request recovers that target and completes. F3 exact
// disposal removes the incomplete/completed target and response-loss replay is
// an idempotent success. The durable checkpoint reference remains store-owned.
#[tokio::test]
async fn corrupt_checkpoint_is_rejected_before_restore() {
    let restored_disposal = restored_disposal_authorization();
    let authority = pc::test_support::EffectAuthority::new(restored_disposal.effect_fence());

    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("checkpoint-session", false);
    let sandbox = provider.create_sandbox(&spec).await.unwrap();
    std::fs::write(sandbox.workspace_path().join("value"), b"original").unwrap();
    let store = CheckpointStore::default();
    let checkpoint_request = checkpoint_request_for(&spec.scope);
    let receipt = sandbox
        .checkpoint(&checkpoint_request, &store)
        .await
        .unwrap();
    sandbox.dispose_unattached().await.unwrap();
    let original = {
        let mut objects = store.objects.lock().unwrap();
        let stored = objects.get_mut(&receipt.id).unwrap();
        let original = stored.bytes.clone();
        stored.bytes = b"corrupt".to_vec();
        original
    };
    let request = exact_restore_request(&spec, &checkpoint_request, &receipt, "corruption");
    let evidence = request.evidence(&spec);
    let restored_root = restore_target::restoration_root(tmp.path(), &evidence).unwrap();
    assert!(
        pc::SandboxProvider::restore(&provider, &spec, &request, &store)
            .await
            .is_err(),
        "F1 corruption fails closed"
    );
    assert!(restored_root.is_dir(), "F1 exact target is retryable");
    assert!(
        !restored_root.join("value").exists(),
        "F1 no checkpoint entry write"
    );

    store
        .objects
        .lock()
        .unwrap()
        .get_mut(&receipt.id)
        .unwrap()
        .bytes = original;
    let recovered = pc::SandboxProvider::restore(&provider, &spec, &request, &store)
        .await
        .expect("F2 exact retry");
    assert_eq!(recovered.evidence(), &evidence, "F2 exact evidence");
    assert_eq!(
        std::fs::read(restored_root.join("value")).unwrap(),
        b"original",
        "F2 materialized bytes"
    );
    drop(recovered);
    pc::SandboxProvider::dispose_restored(
        &provider,
        &spec,
        &request,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("F3 exact disposal");
    pc::SandboxProvider::dispose_restored(
        &provider,
        &spec,
        &request,
        &restored_disposal,
        &authority,
    )
    .await
    .expect("F3 disposal response-loss replay");
}
