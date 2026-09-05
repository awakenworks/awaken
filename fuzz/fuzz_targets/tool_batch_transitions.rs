#![no_main]

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::tool::{ToolCall, ToolOutput, ToolRecoveryMode, ToolRecoveryPolicy};
use awaken_runtime_contract::tool_batch::{ToolBatch, ToolBatchPhase, ToolWaitKind};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Causes: arbitrary command order, selected/missing call, correct/wrong
    // approval correlation, and a terminal/finalized predecessor. Effects:
    // F1 rejected commands stutter exactly; F2 terminal calls never re-enter;
    // F3 finalized batches are absorbing; F4 every prefix round-trips through
    // the production recovery decoder with stable operation identities.
    // Stateful/metamorphic design replaces a finite decision table here: all
    // byte sequences select legal and illegal command histories, bounded to
    // 128 commands and two calls per fuzz input. The oracle compares serialized
    // predecessors and invariant observations, not the production guard table.
    let policy = ToolRecoveryPolicy::try_new(ToolRecoveryMode::Idempotent, 3).unwrap();
    let mut batch = ToolBatch::for_step(
        RunId("fuzz-run".into()),
        0,
        ["a", "b"].map(|id| {
            (
                ToolCall {
                    call_id: id.into(),
                    tool_id: "fuzz-tool".into(),
                    arguments: serde_json::json!({}),
                },
                policy.clone(),
            )
        }),
    )
    .unwrap();
    let identities = ["a", "b"].map(|id| batch.operation_id(id));
    for command in data.iter().take(128) {
        let before = serde_json::to_value(&batch).unwrap();
        let terminal: Vec<_> = batch
            .calls()
            .iter()
            .map(|c| c.phase.is_terminal())
            .collect();
        let finalized = batch.phase() == ToolBatchPhase::Finalized;
        let id = ["a", "b", "missing"][usize::from(command / 16) % 3];
        let output = || ToolOutput::ok(id, "result");
        let result = match command % 10 {
            0 => batch.mark_executing(id).map(|_| ()),
            1 => batch.mark_awaiting(id, ToolWaitKind::ToolPermission, "ticket"),
            2 => batch
                .resume_executing(id, ToolWaitKind::ToolPermission, "ticket")
                .map(|_| ()),
            3 => batch
                .resume_executing(id, ToolWaitKind::ToolPermission, "wrong")
                .map(|_| ()),
            4 => batch.complete(output()),
            5 => batch.complete_immediate(output()),
            6 => batch.mark_indeterminate(id, "unknown"),
            7 => batch.finalize(),
            8 => batch.set_result_messages(id, Vec::new()),
            _ => {
                batch.seal_on_run_end("ended");
                Ok(())
            }
        };
        let after = serde_json::to_value(&batch).unwrap();
        if result.is_err() || finalized {
            assert_eq!(
                before, after,
                "F1/F3 rejection or finalized state must stutter"
            );
        }
        for (was_terminal, call) in terminal.iter().zip(batch.calls()) {
            assert!(
                !was_terminal || call.phase.is_terminal(),
                "F2 terminal absorption"
            );
        }
        assert_eq!(
            identities,
            ["a", "b"].map(|id| batch.operation_id(id)),
            "F4 stable identity"
        );
        let recovered: ToolBatch = serde_json::from_value(after.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(recovered).unwrap(),
            after,
            "F4 recovery"
        );
    }
});
