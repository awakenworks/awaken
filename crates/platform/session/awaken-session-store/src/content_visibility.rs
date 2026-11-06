// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Both selected SQL stores decode one live-root/tombstone snapshot through this
// @formal reason: kernel. Atomic deletion persists the bit derived from the original root; this
// @formal reason: is storage-boundary classification, not a SQL atomicity proof claim.
//! Linked content retains its Session's privacy across atomic deletion.
use super::{EncodedSessionRow, SessionRepositoryError, corrupt, decode};

/// One UNION snapshot distinguishes the live root from its retained deletion
/// evidence. A mixed identity must never be resolved by taking the first row.
pub(crate) fn workspace_content_is_public(
    owner_scope: &str,
    session_id: &str,
    rows: Vec<(String, i64, Option<String>, Option<i64>)>,
) -> Result<bool, SessionRepositoryError> {
    let mut rows = rows.into_iter();
    let Some((owner, revision, aggregate_json, private_content)) = rows.next() else {
        return Ok(false);
    };
    if rows.next().is_some() || owner.trim().is_empty() || revision <= 0 {
        return Err(corrupt("invalid Session content visibility identity"));
    }
    if owner != owner_scope {
        return Ok(false);
    }
    match (aggregate_json, private_content) {
        (Some(aggregate_json), None) => {
            let root = decode(EncodedSessionRow {
                aggregate_json,
                revision,
            })
            .map_err(corrupt)?;
            if root.session_id.as_str() != session_id {
                return Err(corrupt("Session content visibility aggregate id mismatch"));
            }
            Ok(awaken_session_contract::SessionPrivateBinding::of(&root).is_none())
        }
        (None, Some(private_content @ (0 | 1))) => Ok(private_content == 0),
        // Historical evidence and an older writer's omitted field establish
        // no ordinary visibility. Never turn an unknown deletion into public.
        (None, None) => Ok(false),
        _ => Err(corrupt("invalid Session content visibility evidence")),
    }
}
