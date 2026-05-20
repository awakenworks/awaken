//! `ToolCallResume` + `ResumeDecisionAction` — pins the suspension/HITL
//! payload shape `reference/tool-execution-modes.md` cites.

use awaken::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use serde_json::json;

fn main() {
    let resume = ToolCallResume {
        decision_id: "dec-123".into(),
        action: ResumeDecisionAction::Resume,
        result: json!({ "approved": true }),
        reason: Some("operator approved".into()),
        updated_at: 1_700_000_000_000,
    };

    assert_eq!(resume.action, ResumeDecisionAction::Resume);

    // Wire round-trip — the exact body `POST /v1/runs/:id/decision` takes.
    let bytes = serde_json::to_vec(&resume).expect("encode");
    let parsed: ToolCallResume = serde_json::from_slice(&bytes).expect("decode");
    assert_eq!(parsed.decision_id, "dec-123");
    assert_eq!(parsed.reason.as_deref(), Some("operator approved"));

    // Cancel variant exists; ensure construction doesn't drift.
    let cancel = ToolCallResume {
        decision_id: "dec-124".into(),
        action: ResumeDecisionAction::Cancel,
        result: serde_json::Value::Null,
        reason: None,
        updated_at: 1_700_000_000_001,
    };
    assert!(matches!(cancel.action, ResumeDecisionAction::Cancel));
}
