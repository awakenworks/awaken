//! Construct one variant of each major error enum the runtime surfaces —
//! pins the message shapes `reference/errors.md` cites for `ToolError`,
//! `StorageError`, and `ResolveError`.

use awaken::contract::storage::StorageError;
use awaken::contract::tool::ToolError;
use awaken_runtime::registry::resolve::ResolveError;

fn main() {
    // ToolError — message format is the user-visible contract.
    let te = ToolError::InvalidArguments("missing `x`".into());
    assert_eq!(te.to_string(), "Invalid arguments: missing `x`");

    let cancelled = ToolError::Cancelled("user cancelled".into());
    assert!(cancelled.to_string().contains("Cancelled"));

    // StorageError — version conflict is the load-bearing one for replays.
    let nf = StorageError::NotFound("thread-123".into());
    assert!(nf.to_string().contains("thread-123"));
    let conflict = StorageError::VersionConflict {
        expected: 1,
        actual: 2,
    };
    assert!(conflict.to_string().contains("1"));

    // ResolveError — `agent not found: <id>` is what `/v1/runs` returns
    // on a typo in `agent_id`.
    let re = ResolveError::AgentNotFound("worker".into());
    assert_eq!(re.to_string(), "agent not found: worker");
}
