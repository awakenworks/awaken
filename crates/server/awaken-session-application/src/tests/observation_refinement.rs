//! Actual durable root observations consumed by the existing Rust-to-TLA gate.
use super::*;

pub(super) fn root_observation(
    before: Option<&PersistedSession>,
    after: Option<&PersistedSession>,
    now: u64,
    rule: &str,
) -> serde_json::Value {
    let state = |root: Option<&PersistedSession>| serde_json::json!({"exists": root.is_some(), "revision": root.map_or(0, |s| s.revision.0), "created": root.and_then(|s| s.created_at_unix_ms), "updated": root.and_then(|s| s.updated_at_unix_ms)});
    serde_json::json!({"rule": rule, "before": state(before), "after": state(after), "now": now})
}

pub(super) fn root_step(name: &str, before: &PersistedSession, after: &PersistedSession, now: u64) {
    awaken_reliability_testkit::write_observation_trace(
        name,
        "root",
        serde_json::json!([root_observation(Some(before), Some(after), now, name)]),
    );
}

/// Read the canonical aggregate/outbox, retaining exact bytes for retry frames.
/// Cleanup's private progress wrappers serialize around the same `cleanup`
/// member; this projects their existing effect id, never derives a second one.
pub(super) async fn deletion_observation(
    repo: &dyn ManagedSessionRepository,
    session_id: &str,
    owner_scope: &str,
) -> serde_json::Value {
    let root = match repo
        .get(&awaken_session_contract::SessionId::from(session_id))
        .await
    {
        Ok(root) => Some(root),
        Err(awaken_session_contract::SessionRepositoryError::NotFound) => None,
        Err(error) => panic!("deletion observation failed: {error}"),
    };
    let tombstone = repo
        .deleted_event_admissions(
            owner_scope,
            &awaken_session_contract::SessionId::from(session_id),
        )
        .await
        .expect("deletion evidence");
    let owner = if root.is_some() {
        assert!(tombstone.is_none(), "live root has no tombstone");
        repo.owner(&awaken_session_contract::SessionId::from(session_id))
            .await
            .expect("durable owner")
    } else {
        assert!(
            tombstone.is_some(),
            "D5 absence requires durable tombstone evidence"
        );
        owner_scope.to_string()
    };
    let facts = repo.pending_lifecycle().await.expect("deletion outbox");
    let mut cleanup = root
        .as_ref()
        .map(|root| serde_json::to_value(&root.terminal_cleanup).unwrap());
    while let Some(inner) = cleanup
        .as_ref()
        .and_then(|value| value.get("cleanup"))
        .cloned()
    {
        cleanup = Some(inner);
    }
    let effect = cleanup
        .as_ref()
        .and_then(|value| value.get("effect_id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("none");
    let phase = root.as_ref().map_or("completed", |root| {
        let cleanup = &root.terminal_cleanup;
        if cleanup.is_not_requested() {
            "not_requested"
        } else if cleanup.is_fenced() {
            "fenced"
        } else if cleanup.is_completed() {
            "completed"
        } else {
            assert!(cleanup.is_requested());
            "requested"
        }
    });
    let disposition = root
        .as_ref()
        .map_or("tombstoned", |root| match root.disposition {
            awaken_session_contract::SessionDisposition::Active => "active",
            awaken_session_contract::SessionDisposition::Archived { .. } => "archived",
            awaken_session_contract::SessionDisposition::Deleting => "deleting",
            awaken_session_contract::SessionDisposition::Deleted => "tombstoned",
        });
    serde_json::json!({
        "disposition": disposition, "terminal": root.as_ref().is_none_or(|root| root.execution.is_terminal()),
        "phase": phase, "effect": effect,
        "cleanup": root.as_ref().map(|root| serde_json::to_string(&root.terminal_cleanup).unwrap()).unwrap_or_else(|| "absent".into()),
        "revision": root.as_ref().map_or_else(|| tombstone.as_ref().unwrap().deleted_revision.0, |root| root.revision.0),
        "owner": owner, "tombstone": serde_json::to_string(&tombstone.as_ref().map(|evidence| serde_json::json!({"deleted_revision": evidence.deleted_revision.0, "admissions": evidence.admissions}))).unwrap(),
        "facts": facts.iter().filter(|fact| fact.event_type == "session.deleted" && fact.object_id == session_id).count(),
        "root": serde_json::to_string(&root).unwrap(), "outbox": serde_json::to_string(&facts).unwrap(),
    })
}
