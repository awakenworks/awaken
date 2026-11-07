//! The request-local Session admission, realization and creation paths cross
//! the heap where each oversized state machine is created.
use super::*;
use std::mem::{size_of, size_of_val};

#[tokio::test]
async fn request_path_boundary_futures_are_one_heap_pointer() {
    // Causes: C1 a Managed request polls Event-batch drive -> Run admission ->
    // projection recovery -> Resource reconciliation -> realization inline on
    // the caller's thread; C2 each listed boundary returns its future already
    // boxed (`BoxFuture`), not an inline `async fn` state machine.
    // Effect: E1 every listed future is exactly one boxed pointer, so the
    // awaiting poll frame reserves no slot for the callee's state machine.
    //
    // | Rule | C1 | C2 | Effect |
    // |---|---|---|---|
    // | R1 | inline | boxed | E1 |
    // | R2 | inline | `async fn` | E1 fails: the callee state is inline |
    //
    // An unoptimized poll frame reserves two slots for every awaited inline
    // callee (the call result and its `IntoFuture` value) and one for a
    // call-site `Box::pin`, and never reuses them. Before these boundaries the
    // callees measured 21-53 KB (admit_run_session 35,472 B,
    // recover_session_projection 29,488 B, reconcile_persisted_resources
    // 38,160 B, realize_session_after_refresh 23,696 B) and their callers'
    // poll frames 79-184 KiB, so one POST of a Session User Event needed
    // 2.0-2.06 MiB and overflowed the 2 MiB default test-thread stack in
    // `awaken-process-startup::resource_scheduling`.
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let app = application(repository, Arc::new(RecordingEnvironmentSource::default()));
    let boxed = size_of::<futures_util::future::BoxFuture<'static, ()>>();
    let session_id = SessionId::from("stack-session");
    let session = persisted("stack-session", false, "idle");

    // Futures are only constructed and dropped; nothing is polled.
    let sizes = [
        (
            "admit_run_session",
            size_of_val(&app.run_admissions.admit_run_session(
                "stack-ws",
                "stack-session",
                "agent",
            )),
        ),
        (
            "recover_session_projection",
            size_of_val(
                &app.run_admissions
                    .recover_session_projection("stack-session", Some("stack-ws")),
            ),
        ),
        (
            "append_and_drive_session_event_batch_idempotent",
            size_of_val(
                &app.event_batches
                    .append_and_drive_session_event_batch_idempotent(
                        &session_id,
                        Vec::new(),
                        None,
                        None,
                        None,
                    ),
            ),
        ),
        (
            "reconcile_persisted_resources",
            size_of_val(
                &app.resource_reconciliation
                    .reconcile_persisted_resources("stack-ws", session.clone()),
            ),
        ),
        (
            "ensure_repository_credentials_pinned",
            size_of_val(
                &app.credentials
                    .ensure_repository_credentials_pinned("stack-ws", session),
            ),
        ),
        (
            "realize_session_after_refresh",
            size_of_val(&app.realizations.realize_session_after_refresh(&session_id)),
        ),
    ];
    for (boundary, size) in sizes {
        assert_eq!(size, boxed, "R1 {boundary} returns one boxed future");
    }
}
