use super::*;
use crate::worker::ReservationRecoveryContext;
use awaken_dispatch_erasure::{AnyCoordinatorDispatch, DispatchBinding};
use std::mem::size_of_val;

fn stack_request(run: &str) -> crate::RunDispatch {
    crate::RunDispatch::new(RunActivation::new(
        RunId(run.into()),
        ThreadId(format!("{run}-thread")),
        ExecutableAgentSnapshot::builder("stack-snapshot")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse("test", "test", "native")
                    .expect("complete model binding"),
            )
            .build(),
        Vec::new(),
    ))
}

#[tokio::test]
async fn claim_path_futures_stay_small_enough_for_a_default_thread_stack() {
    // Causes: C1 every claim entry reaches the drive body through the heap
    // boundary `drive_claimed_with_cancellation`; C2 the terminal settlement
    // operations awaited at many drive sites return heap `ClaimFuture`s.
    // Effects: E1 the boundary is one pointer pair and no claim-entry future
    // embeds the drive body; E2 each settlement future is one pointer pair, so
    // the body's poll frame holds no settlement state machine.
    //
    // | Rule | C1 | C2 | Effect |
    // |---|---|---|---|
    // | R1 | heap | heap | E1 + E2 |
    // | R2 | inline | heap | E1 fails: an entry holds the body plus its Claimed |
    // | R3 | heap | inline | E2 fails: a settlement is an inline state machine |
    //
    // An inline boundary keeps its own `Claimed` argument beside the body it
    // awaits, so any entry that embeds the body is at least
    // `body + size_of::<Claimed>()`. Before both boundaries an
    // unoptimized build measured the body at 22,800 bytes and entries at
    // 27-40 KiB; an unoptimized poll frame reserves a slot for each awaited
    // callee future, which grew the body frame to 329 KiB and the entry frames
    // to 58-85 KiB until a Session child Run overflowed its 2 MiB test-thread
    // stack beneath the Runtime attempt.
    let backend = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    let claim = |run: &'static str| {
        let backend = backend.clone();
        async move {
            backend
                .claim_new_run(stack_request(run), "worker", 1_000, &Default::default())
                .await
                .expect("fixture admission")
                .expect("fixture claim")
        }
    };
    let drive_claim = claim("stack-drive").await;
    let body_claim = claim("stack-body").await;
    let worker = DispatchWorker::new_bound(
        Arc::new(Runtime::new()),
        DispatchBinding::local(Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(
            backend.clone(),
        ))),
        Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock())),
        "worker",
    );
    let clock = awaken_time::system_clock;
    let input = PendingInput {
        message_id: "stack-message".into(),
        run_id: RunId("stack-resume".into()),
        thread_id: ThreadId("stack-resume-thread".into()),
        correlation_id: "stack-correlation".into(),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input("continue".into()),
    };
    let tick_run = RunId("stack-tick".into());
    let consumed: Vec<String> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let failure = Error::Execution(awaken_runtime_contract::execution::Error::Execution(
        "stack".into(),
    ));

    // Futures are only constructed and dropped; nothing is polled.
    let body = size_of_val(&worker.drive_claimed_inner(
        body_claim.clone(),
        clock(),
        AttemptCancellation::default(),
        None,
    ));
    let embedded_body = body + size_of_val(&body_claim);
    let entries = [
        (
            "start_run",
            size_of_val(&worker.start_run(stack_request("stack-start"), clock())),
        ),
        (
            "resume_run",
            size_of_val(&worker.resume_run(input, clock())),
        ),
        ("tick", size_of_val(&worker.tick(clock()))),
        (
            "tick_run",
            size_of_val(&worker.tick_run(&tick_run, clock())),
        ),
        (
            "drive_claimed",
            size_of_val(&worker.drive_claimed(drive_claim, clock())),
        ),
    ];
    for (entry, size) in entries {
        assert!(
            size < embedded_body,
            "R2: {entry} future is {size} bytes, at least the {body}-byte drive body \
             plus its Claimed; the body must stay behind its heap boundary"
        );
    }

    let boxed = std::mem::size_of::<crate::worker::ClaimFuture<'static, ()>>();
    let reservation_clock = clock();
    let heap_boundaries = [
        (
            "recover_session_run_reservation",
            size_of_val(&worker.reservation_recovery.recover(
                &body_claim,
                reservation_clock.as_ref(),
                ReservationRecoveryContext {
                    store: worker.store.as_ref(),
                    execution_authority: &worker.ports,
                    settlement_observer: None,
                    recovery_source: None,
                    reservation_ttl_ms: worker.lease_ms,
                },
            )),
        ),
        (
            "drive_claimed_with_cancellation",
            size_of_val(&worker.drive_claimed_with_cancellation(
                body_claim.clone(),
                clock(),
                AttemptCancellation::default(),
                None,
            )),
        ),
        (
            "settle",
            size_of_val(&worker.settle(&body_claim, None, DispatchOutcome::Done, &consumed)),
        ),
        (
            "settle_from_committed_terminal",
            size_of_val(&worker.settle_from_committed_terminal(&body_claim, &mut pending)),
        ),
        (
            "settle_if_terminal_or_raise",
            size_of_val(&worker.settle_if_terminal_or_raise(&body_claim, &consumed, failure)),
        ),
        (
            "end_claimed_before_execution",
            size_of_val(&worker.end_claimed_before_execution(&body_claim, EndCause::Indeterminate)),
        ),
    ];
    for (boundary, size) in heap_boundaries {
        assert_eq!(
            size, boxed,
            "R2/R3: {boundary} must return a heap ClaimFuture, not an inline state machine"
        );
    }
}
