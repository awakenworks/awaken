//! `RunIdentity` + `RunStatus` + `Phase` — pins the lifecycle surface
//! `explanation/run-lifecycle-and-phases.md` cites.

use awaken::Phase;
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::lifecycle::RunStatus;

fn main() {
    // RunIdentity::new arity matches the constructor docs cite.
    let identity = RunIdentity::new(
        "thread-1".into(),
        Some("parent-thread".into()),
        "run-1".into(),
        Some("parent-run".into()),
        "agent-1".into(),
        RunOrigin::User,
    );
    assert_eq!(identity.run.thread_id, "thread-1");
    assert_eq!(identity.run.agent_id, "agent-1");
    assert_eq!(identity.execution.origin, RunOrigin::User);

    // Serde round-trip — the wire shape on /v1/threads/:id/runs.
    let json = serde_json::to_value(&identity).expect("encode");
    let parsed: RunIdentity = serde_json::from_value(json).expect("decode");
    assert_eq!(parsed.run.thread_id, "thread-1");

    // RunStatus default + variants — what /v1/runs/:id returns.
    let s = RunStatus::default();
    assert!(matches!(s, RunStatus::Running));
    let _created = RunStatus::Created;

    // Phase is the typed enum the 9-phase loop walks; ALL is the canonical
    // execution order.
    assert_eq!(Phase::ALL.len(), 9);
    assert_eq!(Phase::ALL[0], Phase::RunStart);
    assert_eq!(Phase::ALL[8], Phase::RunEnd);
}
