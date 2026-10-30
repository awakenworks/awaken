//! T33-C03 (cross-adapter consistency), Worker Hand relay row: the shared
//! injected upstream fault, in the tool vocabulary (see
//! `awaken-protocol-mcp/tests/cross_adapter_fault_mcp.rs`), fails inside a
//! production `HandSession` served by `serve_hand` and returns over the framed
//! byte channel to the production brain-side `RemoteToolExecutor`.
//!
//! | fault | Hand-side `ToolError`       | expected brain-side observation        |
//! | F1    | `UnavailableBeforeDispatch` | same: certain, retry-safe              |
//! | F2    | `Indeterminate`             | same: uncertain, never repeated        |
//! | F3    | `DefinitiveFailure`         | same: certain, not retryable, account  |
//!
//! Oracles: O0 authority/uncertainty/retry hint preserved; O1 never success.
//! Holds: O1 for every fault; O0 for F2 and F3. Gap: `HandSession::dispatch`
//! folds every non-Indeterminate error into `HandErrorKind::Execution`, which
//! the executor maps to `DefinitiveFailure`, so F1's retry-safe hint is lost;
//! the ignored reproducer asserts the correct classification.
//! Negative control (executed): a Hand-side mutant that swallows the fault
//! into a successful output crosses the same relay; the same checker must
//! flag the brain's observation as success.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError, ToolExecutor, ToolOutput};
use awaken_tool_relay::{FsOperationLedger, HandSession, RemoteToolExecutor, serve_hand};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// A process-unique ledger directory removed on drop.
struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn create() -> Self {
        let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "awaken-t33-c03-hand-relay-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create ledger test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InjectedFault {
    Retryable,
    Unknown,
    Definitive,
}

impl InjectedFault {
    const ALL: [Self; 3] = [Self::Retryable, Self::Unknown, Self::Definitive];

    fn tool(self) -> &'static str {
        match self {
            Self::Retryable => "fault_retryable",
            Self::Unknown => "fault_unknown",
            Self::Definitive => "fault_definitive",
        }
    }

    fn detail(self) -> String {
        format!(
            "upstream 529 overloaded_error (retry-after 7s) [{}]",
            self.tool()
        )
    }

    fn origin_error(self) -> ToolError {
        match self {
            Self::Retryable => ToolError::UnavailableBeforeDispatch(self.detail()),
            Self::Unknown => ToolError::Indeterminate(self.detail()),
            Self::Definitive => ToolError::DefinitiveFailure(self.detail()),
        }
    }

    fn expected(self) -> Classification {
        match self {
            Self::Retryable => Classification::failure("unavailable_before_dispatch", false, true),
            Self::Unknown => Classification::failure("indeterminate", true, false),
            Self::Definitive => Classification::failure("definitive_failure", false, false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Classification {
    authority: String,
    uncertain: bool,
    retryable: bool,
    success: bool,
}

impl Classification {
    fn failure(authority: &str, uncertain: bool, retryable: bool) -> Self {
        Self {
            authority: authority.to_string(),
            uncertain,
            retryable,
            success: false,
        }
    }
}

fn classify(result: &Result<ToolOutput, ToolError>) -> Classification {
    match result {
        Ok(output) if output.is_error => Classification::failure("tool_error_result", false, false),
        Ok(_) => Classification {
            authority: "success".to_string(),
            uncertain: false,
            retryable: false,
            success: true,
        },
        Err(ToolError::UnavailableBeforeDispatch(_)) => {
            Classification::failure("unavailable_before_dispatch", false, true)
        }
        Err(ToolError::Indeterminate(_)) => Classification::failure("indeterminate", true, false),
        Err(ToolError::DefinitiveFailure(_)) => {
            Classification::failure("definitive_failure", false, false)
        }
        Err(ToolError::StateInvalidatedAfterDispatch(_)) => {
            Classification::failure("state_invalidated_after_dispatch", false, false)
        }
        Err(other) => Classification::failure(&format!("other: {other}"), false, false),
    }
}

fn check_not_success(adapter: &str, observed: &Classification) -> Result<(), String> {
    if observed.success {
        return Err(format!(
            "O1 {adapter}: the injected fault surfaced as success: {observed:?}"
        ));
    }
    Ok(())
}

fn check(
    adapter: &str,
    expected: &Classification,
    observed: &Classification,
) -> Result<(), String> {
    check_not_success(adapter, observed)?;
    if observed != expected {
        return Err(format!(
            "O0 {adapter}: expected {expected:?}, observed {observed:?}"
        ));
    }
    Ok(())
}

struct FaultTool(InjectedFault);

#[async_trait]
impl RawTool for FaultTool {
    fn id(&self) -> &str {
        self.0.tool()
    }

    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        Err(self.0.origin_error())
    }
}

/// Negative-control Hand adapter: swallows the fault into success.
struct SwallowingTool(Arc<dyn RawTool>);

#[async_trait]
impl RawTool for SwallowingTool {
    fn id(&self) -> &str {
        self.0.id()
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let call_id = call.call_id.clone();
        match self.0.invoke(call).await {
            Ok(output) => Ok(output),
            Err(_) => Ok(ToolOutput::ok(call_id, "swallowed".to_string())),
        }
    }
}

/// Invoke `fault` once through a fresh Hand relay and return the brain's view.
async fn invoke_through_relay(
    fault: InjectedFault,
    swallow: bool,
) -> Result<ToolOutput, ToolError> {
    let directory = TestDirectory::create();
    let ledger =
        Arc::new(FsOperationLedger::open(directory.0.clone()).expect("open operation ledger"));
    let tool: Arc<dyn RawTool> = Arc::new(FaultTool(fault));
    let tool: Arc<dyn RawTool> = if swallow {
        Arc::new(SwallowingTool(tool))
    } else {
        tool
    };
    let session = HandSession::new([tool], ledger, awaken_time::system_clock());
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));
    let executor = RemoteToolExecutor::new(brain_end);
    let result = executor
        .invoke(&ToolCall {
            call_id: format!("t33-c03-{}-{swallow}", fault.tool()),
            tool_id: fault.tool().to_string(),
            arguments: serde_json::json!({}),
        })
        .await;
    drop(executor);
    let _ = hand.await;
    drop(directory);
    result
}

#[tokio::test]
async fn hand_relay_never_reports_an_injected_tool_fault_as_success() {
    for fault in InjectedFault::ALL {
        let result = invoke_through_relay(fault, false).await;
        check_not_success("hand-relay", &classify(&result))
            .unwrap_or_else(|violation| panic!("{fault:?}: {violation}"));
        assert!(
            result.is_err(),
            "{fault:?}: a typed tool error, never a ToolOutput"
        );
    }
    for fault in [InjectedFault::Unknown, InjectedFault::Definitive] {
        let result = invoke_through_relay(fault, false).await;
        check("hand-relay", &fault.expected(), &classify(&result))
            .unwrap_or_else(|violation| panic!("{fault:?}: {violation}"));
    }
    let definitive = invoke_through_relay(InjectedFault::Definitive, false).await;
    let Err(ToolError::DefinitiveFailure(message)) = &definitive else {
        panic!("F3: expected DefinitiveFailure, got {definitive:?}");
    };
    assert!(
        message.contains(&InjectedFault::Definitive.detail()),
        "F3: the upstream's account crosses the relay: {message}"
    );
}

#[tokio::test]
async fn hand_relay_fault_checker_detects_a_hand_adapter_that_swallows_the_fault() {
    for fault in InjectedFault::ALL {
        let result = invoke_through_relay(fault, true).await;
        let observed = classify(&result);
        assert!(
            observed.success,
            "{fault:?}: the mutant really reported success"
        );
        let violation = check_not_success("hand-relay-mutant", &observed)
            .expect_err("the checker detects the swallowed fault");
        assert!(violation.starts_with("O1"), "{fault:?}: {violation}");
    }
}

#[tokio::test]
#[ignore = "product gap: HandSession::dispatch folds UnavailableBeforeDispatch into HandErrorKind::Execution, which RemoteToolExecutor maps to DefinitiveFailure, losing the retry-safe hint"]
async fn hand_relay_keeps_the_retry_safe_classification_of_an_undispatched_fault() {
    let result = invoke_through_relay(InjectedFault::Retryable, false).await;
    check(
        "hand-relay",
        &InjectedFault::Retryable.expected(),
        &classify(&result),
    )
    .unwrap_or_else(|violation| panic!("F1: {violation}"));
}
