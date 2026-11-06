//! Stack bound for a Run started beneath another Run.
//!
//! A hook may start an auxiliary Run (a compaction summary, a judge, a memory
//! extraction) while the outer Run's loop is still on the stack. Unoptimized
//! builds give every poll function a slot for each callee future it awaits
//! inline, so a public entry future that embeds the whole Native loop puts a
//! copy of that loop's state into every caller frame on the way down; two
//! nested Runs then overflowed a 2 MiB test thread.

use std::mem::size_of_val;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

/// Clippy's `large_futures` threshold: a future at least this large is one the
/// lint asks to box rather than await inline.
const LARGE_FUTURE: usize = 16 * 1024;

#[test]
fn nested_run_entry_futures_do_not_embed_the_native_loop() {
    // Causes: C1 the Native loop (`drive`) crosses one heap boundary; C2 its
    // step phases (inference, tool execution, batch recovery) cross the same
    // boundary. Effects: E1 every public entry an auxiliary Run awaits stays
    // below the large-future threshold, so a caller frame holds no copy of
    // the loop's state.
    //
    // | Rule | C1 | C2 | Effect |
    // |---|---|---|---|
    // | R1 | heap | heap | E1 |
    // | R2 | inline | heap | E1 fails: the entry embeds the whole loop |
    // | R3 | heap | inline | E1 fails once a phase outgrows the threshold |
    //
    // Before the boundary `run_to_completion` measured 32,744 bytes and the
    // `Runtime::execute` poll frame 166 KiB in an unoptimized build; with it,
    // `run_to_completion` measures 13,224 bytes.
    let runtime = Runtime::new();
    let config = ExecutableAgentSnapshot::builder("assistant")
        .model(ModelBinding::parse("demo", "stub", "stub").expect("complete model binding"))
        .build();
    // Futures are only constructed and dropped; nothing is polled.
    let entries = [
        (
            "run_to_completion",
            size_of_val(&runtime.run_to_completion(
                &config,
                "thread",
                "input",
                RuntimeRunContext::new(),
                |_| ResumeResult::allow(),
            )),
        ),
        (
            "run_to_completion_with_id",
            size_of_val(&runtime.run_to_completion_with_id(
                &config,
                RunId("run".into()),
                "thread",
                "input",
                RuntimeRunContext::new(),
                |_| ResumeResult::allow(),
            )),
        ),
        (
            "run",
            size_of_val(&runtime.run(&config, "input", RuntimeRunContext::new())),
        ),
    ];
    for (entry, size) in entries {
        assert!(
            size < LARGE_FUTURE,
            "R1: `{entry}` future is {size} bytes; an auxiliary Run's entry must hold the \
             Native loop and its step phases behind their heap boundary"
        );
    }
}
