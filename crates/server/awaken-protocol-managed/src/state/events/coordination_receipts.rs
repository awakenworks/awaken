//! Disposable index of the original primary coordination-result selector.
use super::*;
use std::collections::{HashMap, HashSet};

pub(super) struct CoordinationReceipts {
    by_call: HashMap<String, Option<String>>,
    #[cfg(test)]
    parsed: usize,
}
impl CoordinationReceipts {
    pub(super) fn for_calls(
        events: &[Event],
        owners: &HashMap<String, String>,
        wanted: &HashSet<&str>,
    ) -> Self {
        let mut by_call = HashMap::new();
        #[cfg(test)]
        let mut parsed = 0;
        if wanted.is_empty() {
            return Self {
                by_call,
                #[cfg(test)]
                parsed,
            };
        }
        for event in events {
            if owners.contains_key(&event.id) {
                continue;
            }
            let OutboundKind::AgentToolResult {
                tool_use_id,
                content,
                is_error,
            } = &event.kind
            else {
                continue;
            };
            if *is_error == Some(true) || !wanted.contains(tool_use_id.as_str()) {
                continue;
            }
            // Record a malformed/unaccepted FIRST result as None too. Choosing
            // a later valid result would widen the former find-then-parse rule.
            by_call.entry(tool_use_id.clone()).or_insert_with(|| {
                #[cfg(test)]
                {
                    parsed += 1;
                }
                let receipt =
                    serde_json::from_str::<serde_json::Value>(&content_text(content)).ok()?;
                if receipt.get("accepted").and_then(serde_json::Value::as_bool) != Some(true) {
                    return None;
                }
                receipt
                    .get("session_thread_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
        }
        Self {
            by_call,
            #[cfg(test)]
            parsed,
        }
    }
    pub(super) fn get(&self, call: &str) -> Option<&str> {
        self.by_call.get(call)?.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn result(id: &str, call: &str, text: &str, error: bool) -> Event {
        Event {
            id: id.into(),
            processed_at: None,
            kind: OutboundKind::AgentToolResult {
                tool_use_id: call.into(),
                content: vec![ContentBlock::text(text)],
                is_error: Some(error),
            },
        }
    }
    #[test]
    fn receipt_index_preserves_first_primary_result_and_never_promotes_later_text() {
        // C1 result belongs to primary vs child; C2 failed vs non-error;
        // C3 first eligible JSON is malformed/rejected/accepted; C4 a later
        // eligible result is accepted; C5 caller did not request this call.
        // R1 !C1 or C2 => skip; R2 first malformed/rejected + C4 => still none;
        // R3 first accepted => its exact Thread; R4 C5 => no stored result.
        let accepted = r#"{"accepted":true,"session_thread_id":"child"}"#;
        let events = vec![
            result("owned", "a", accepted, false),
            result("bad", "a", "not json", false),
            result("later", "a", accepted, false),
            result("error", "b", accepted, true),
            result("good", "b", accepted, false),
            result("rejected", "c", r#"{"accepted":false}"#, false),
            result("later-c", "c", accepted, false),
            result("ignored", "unused", accepted, false),
        ];
        let owners = HashMap::from([("owned".into(), "foreign-child".into())]);
        let index =
            CoordinationReceipts::for_calls(&events, &owners, &HashSet::from(["a", "b", "c"]));
        assert_eq!(index.get("a"), None);
        assert_eq!(index.get("b"), Some("child"));
        assert_eq!(index.get("c"), None);
        assert_eq!(index.get("unused"), None);
        assert_eq!(index.by_call.len(), 3);
        assert_eq!(
            index.parsed, 3,
            "only first eligible requested receipts are decoded"
        );
        assert!(
            CoordinationReceipts::for_calls(&events, &owners, &HashSet::new())
                .by_call
                .is_empty()
        );
    }
}
