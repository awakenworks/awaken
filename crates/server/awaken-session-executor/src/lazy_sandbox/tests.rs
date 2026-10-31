//! The deferred Sandbox executor reads its own Session only through its one
//! owner, the realization executor.
//!
//! Decision table for the pinned-Skill lookup and the owner lifetime:
//!
//! | Row | Owner     | Registry | Session                  | Effect                                                        |
//! |-----|-----------|----------|--------------------------|---------------------------------------------------------------|
//! | D1  | live      | healthy  | not installed            | no pinned file (`None`): the call goes on to the Environment  |
//! | D2  | live      | healthy  | installed, no dispatch   | no pinned file (`None`): the call goes on to the Environment  |
//! | D3  | live      | poisoned | installed                | `read` refuses with the poison text, never falls through      |
//! | D4  | released  | -        | -                        | `read` and other tools refuse "Session host is unavailable";  |
//! |     |           |          |                          | recovery is `NonRecoverable`                                  |
//!
//! A served pinned file (live owner, Session dispatch, advertised Skill) is
//! covered end to end by `pinned_read_path_and_text_decision_table` and the
//! other `host::tests::pinned_skill_read` rows.
use std::sync::Arc;

use awaken_tool_contract::tool::{ToolCall, ToolError, ToolExecutor as _, ToolRecoveryCapability};

use super::DeferredSandboxExecutor;

fn call(tool_id: &str) -> ToolCall {
    let arguments = match tool_id {
        "read" => serde_json::json!({"file_path": ".skills/pinned/SKILL.md"}),
        _ => serde_json::json!({"command": "true"}),
    };
    ToolCall {
        call_id: format!("deferred-{tool_id}"),
        tool_id: tool_id.into(),
        arguments,
    }
}

fn read_args() -> awaken_ext_builtin_tools::ReadArgs {
    awaken_tool_contract::tool::parse_tool_args(
        serde_json::json!({"file_path": ".skills/pinned/SKILL.md"}),
    )
    .expect("read arguments")
}

fn refusal_text(error: ToolError) -> String {
    match error {
        ToolError::DefinitiveFailure(message) => message,
        other => panic!("expected a definitive refusal, got {other:?}"),
    }
}

async fn host() -> Arc<crate::ExecutorComponents> {
    crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .expect("real executor composition")
}

#[tokio::test]
async fn deferred_executor_reads_its_session_through_its_one_owner() {
    let host = host().await;
    let executor = DeferredSandboxExecutor::new(&host.realization_executor, "deferred-lookup");

    // D1: an absent Session has no pinned file and is not installed by the read.
    assert!(
        executor
            .read_pinned_skill(&read_args())
            .expect("live owner")
            .is_none(),
        "D1 absent Session"
    );
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .contains("deferred-lookup"),
        "D1 the read installs nothing"
    );

    // D2: an installed Session outside the Session dispatch serves no pin.
    host.run_assembly
        .installed_sessions
        .install("deferred-lookup")
        .expect("healthy registry");
    assert!(
        executor
            .read_pinned_skill(&read_args())
            .expect("live owner")
            .is_none(),
        "D2 no Session dispatch"
    );

    // D3: a poisoned registry refuses the read; it never falls through to
    // realizing an Environment.
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            host.run_assembly
                .installed_sessions
                .modify("deferred-lookup", |_| -> () {
                    panic!("poison the registry")
                })
        }))
        .is_err()
    );
    let poisoned = refusal_text(
        executor
            .read_pinned_skill(&read_args())
            .expect_err("D3 poisoned registry"),
    );
    assert_eq!(poisoned, "installed Sessions lock poisoned", "D3 helper");
    assert_eq!(
        refusal_text(
            executor
                .invoke(&call("read"))
                .await
                .expect_err("D3 read refuses")
        ),
        poisoned,
        "D3 invoke"
    );
    assert_eq!(
        refusal_text(
            executor
                .execution_domain(&call("read"))
                .await
                .expect_err("D3 domain refuses")
        ),
        poisoned,
        "D3 execution domain"
    );
}

#[tokio::test]
async fn deferred_executor_refuses_once_its_owner_is_released() {
    let host = host().await;
    let executor = DeferredSandboxExecutor::new(&host.realization_executor, "deferred-released");
    let owner = Arc::downgrade(&host.realization_executor);
    drop(host);
    assert!(
        owner.upgrade().is_none(),
        "precondition: the executor does not keep its owner alive"
    );

    // D4: every entry refuses with the same text; recovery is not claimed.
    for tool in ["read", "bash"] {
        assert_eq!(
            refusal_text(executor.invoke(&call(tool)).await.expect_err("D4 invoke")),
            "Session host is unavailable",
            "D4 invoke {tool}"
        );
    }
    assert_eq!(
        refusal_text(
            executor
                .execution_domain(&call("read"))
                .await
                .expect_err("D4 domain")
        ),
        "Session host is unavailable",
        "D4 execution domain"
    );
    assert_eq!(
        executor.recovery_capability("bash"),
        ToolRecoveryCapability::NonRecoverable,
        "D4 recovery"
    );
}
