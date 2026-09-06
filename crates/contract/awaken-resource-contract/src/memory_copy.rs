//! Retained copy candidates belong to Memory, not to Files or Session state.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Exact original head; absence means a newly created local path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCopyBase {
    pub id: String,
    pub content_sha256: String,
}

/// An immutable, store-scoped proposal that did not replace the durable head.
/// None content denotes a proposed deletion, NOT an empty file. Identical
/// proposals coalesce across retries; these values are not Session receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCopyConflict {
    pub path: String,
    pub base: Option<MemoryCopyBase>,
    pub content: Option<String>,
}

impl MemoryCopyConflict {
    /// Content-addressed identity includes path, original identity and operation.
    /// The repository additionally scopes it by store. Never use debug output
    /// (which may contain user content) as an error or telemetry field.
    pub fn id(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"awaken-memory-copy-conflict-v1");
        // Presence tags distinguish absent base/deletion from empty content;
        // length framing prevents ambiguous concatenation of user strings.
        for field in [
            Some(self.path.as_str()),
            self.base.as_ref().map(|base| base.id.as_str()),
            self.base.as_ref().map(|base| base.content_sha256.as_str()),
            self.content.as_deref(),
        ] {
            hash.update([u8::from(field.is_some())]);
            if let Some(field) = field {
                hash.update((field.len() as u64).to_be_bytes());
                hash.update(field.as_bytes());
            }
        }
        format!("{:x}", hash.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_identity_preserves_operation_and_literal_bytes() {
        // RC-ID decision table: each path/base-id/base-sha/content/presence
        // change has a distinct identity. Empty content != deletion; UTF-8
        // and separators are logical bytes, not host path normalization.
        // Exact serde roundtrip/retry is stable; unknown authority fields fail.
        let original = MemoryCopyConflict {
            path: "/目录/note.md".into(),
            base: Some(MemoryCopyBase {
                id: "base".into(),
                content_sha256: "a".repeat(64),
            }),
            content: Some("literal \n ${HOME}".into()),
        };
        let mut candidates = vec![original.clone()];
        for axis in ["path", "id", "sha", "content", "empty", "delete", "new"] {
            let mut changed = original.clone();
            match axis {
                "path" => changed.path.push('x'),
                "id" => changed.base.as_mut().unwrap().id.push('x'),
                "sha" => changed.base.as_mut().unwrap().content_sha256 = "b".repeat(64),
                "content" => changed.content.as_mut().unwrap().push('x'),
                "empty" => changed.content = Some(String::new()),
                "delete" => changed.content = None,
                _ => changed.base = None,
            }
            candidates.push(changed);
        }
        assert_eq!(
            candidates
                .iter()
                .map(MemoryCopyConflict::id)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            candidates.len()
        );
        for candidate in candidates {
            let mut wire = serde_json::to_value(&candidate).unwrap();
            let replay: MemoryCopyConflict = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(candidate, replay);
            assert_eq!(candidate.id(), replay.id());
            wire["authority"] = serde_json::json!("foreign");
            assert!(serde_json::from_value::<MemoryCopyConflict>(wire).is_err());
        }
    }
}
