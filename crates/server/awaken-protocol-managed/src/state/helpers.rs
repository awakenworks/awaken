//! Free helpers shared by the [`ManagedState`] event path: rubric normalization,
//! usage projection, and content-block text extraction.

use super::*;
use crate::types::{ServerToolUsage, SessionThreadCacheCreationUsage};

/// Stable public receipt/event identity shared by preparation and the warm/cold
/// projector. It is a projection of canonical operation truth, not another
/// persisted event id or process-local sequence.
#[must_use]
pub(crate) fn durable_inbound_event_id(session_id: &str, operation_id: &str) -> String {
    format!(
        "evt_{}",
        awaken_session_contract::stable_fingerprint(&(
            "managed-durable-inbound-event-v1",
            session_id,
            operation_id,
        ))
    )
}

/// Process-local update/delete notifications use the decimal sequence emitted
/// by `next_event_id`. Durable inbound receipts deliberately share the public
/// `evt_` namespace but carry a stable fingerprint and must remain in canonical
/// committed ordering.
#[must_use]
pub(crate) fn is_transient_event_id(id: &str) -> bool {
    id.strip_prefix("evt_").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

pub(crate) fn lifecycle_fact(
    id: String,
    session_id: &str,
    workspace_id: Option<String>,
    event_type: &str,
) -> ManagedLifecycleFact {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    ManagedLifecycleFact {
        id,
        object_id: session_id.to_string(),
        workspace_id,
        event_type: event_type.to_string(),
        timestamp,
        runtime_interval: None,
    }
}

/// Normalize the typed Managed rubric to the evaluator's text/file reference.
pub(crate) fn rubric_text(rubric: &crate::types::OutcomeRubric) -> String {
    match rubric {
        crate::types::OutcomeRubric::Text { content } => content.clone(),
        crate::types::OutcomeRubric::File { file_id } => file_id.clone(),
    }
}

/// Price committed cumulative usage with the Session's immutable price
/// snapshot. This is a pure wire projection over the contract pricing kernel,
/// not another usage ledger or a lookup against mutable current prices.
fn usage_list_cost_value(
    usage: &SessionUsage,
    price_snapshot: Option<&awaken_session_contract::ManagedListPriceSnapshot>,
) -> Result<Option<crate::types::MonetaryAmount>, RunError> {
    price_snapshot
        .map(|snapshot| {
            let cursor =
                awaken_session_contract::ManagedBudgetUsageCursor::from_session_usage(usage, None)?;
            snapshot.public_usage_list_cost_minor(cursor)
        })
        .transpose()
        .map_err(|error| RunError::unavailable(error.to_string()))
        .map(|amount| {
            amount.map(|amount| crate::types::MonetaryAmount {
                amount: amount.to_string(),
                currency: crate::types::Currency::USD,
            })
        })
}

/// The session's `usage` object (`BetaManagedAgentsSessionUsage`): cumulative input +
/// output (+ prompt-cache) token counts across all Runs. Emitted after a Run.
pub(crate) fn session_usage_value(
    usage: SessionUsage,
    price_snapshot: Option<&awaken_session_contract::ManagedListPriceSnapshot>,
) -> Result<Usage, RunError> {
    let list_cost = usage_list_cost_value(&usage, price_snapshot)?;
    Ok(Usage {
        active_seconds: Some(usage.active_seconds),
        input_tokens: Some(usage.input_tokens),
        output_tokens: Some(usage.output_tokens),
        cache_read_input_tokens: Some(usage.cache_read_tokens),
        cache_creation: Some(SessionThreadCacheCreationUsage {
            ephemeral_1h_input_tokens: None,
            ephemeral_5m_input_tokens: Some(usage.cache_creation_tokens),
        }),
        list_cost,
        server_tool_use: Some(ServerToolUsage {
            web_fetch_requests: Some(usage.web_fetch_requests),
            web_search_requests: Some(usage.web_search_requests),
        }),
    })
}

/// Project one logical Thread's committed cumulative usage. A zero snapshot is
/// kept `null`, matching the optional Managed field for a Thread whose provider
/// has not reported accounting yet; once any counter exists the wire carries the
/// full cumulative token/tool shape. When the Session aggregate owns a frozen
/// price snapshot, the same authority prices this Thread independently; without
/// one the optional cost remains absent rather than consulting mutable prices.
pub(crate) fn session_thread_usage_value(
    usage: SessionUsage,
    price_snapshot: Option<&awaken_session_contract::ManagedListPriceSnapshot>,
) -> Result<Option<crate::types::SessionThreadUsage>, RunError> {
    if usage == SessionUsage::default() {
        return Ok(None);
    }
    let list_cost = usage_list_cost_value(&usage, price_snapshot)?;
    Ok(Some(crate::types::SessionThreadUsage {
        active_seconds: Some(usage.active_seconds),
        cache_creation: Some(SessionThreadCacheCreationUsage {
            ephemeral_1h_input_tokens: None,
            ephemeral_5m_input_tokens: Some(usage.cache_creation_tokens),
        }),
        cache_read_input_tokens: Some(usage.cache_read_tokens),
        input_tokens: Some(usage.input_tokens),
        output_tokens: Some(usage.output_tokens),
        list_cost,
        server_tool_use: Some(ServerToolUsage {
            web_fetch_requests: Some(usage.web_fetch_requests),
            web_search_requests: Some(usage.web_search_requests),
        }),
    }))
}

/// Concatenate the text of a content-block list.
pub(crate) fn content_text(
    content: &[awaken_agent_contract::agent::content::ContentBlock],
) -> String {
    use awaken_agent_contract::agent::content::ContentBlock;
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn transient_event_ids_exclude_durable_inbound_fingerprints() {
        // Cause/effect graph: C1 next_event_id emits a decimal process-local
        // suffix; C2 an accepted command emits a stable fingerprint in the same
        // public namespace; C3 a malformed/empty suffix is supplied. Effects:
        // E1 only C1 is an overlay; E2 C2 remains canonically ordered durable
        // history; E3 C3 is never reclassified as a local notification.
        // Decision rules T1=C1=>E1, T2=C2=>E2, T3=C3=>E3.
        assert!(is_transient_event_id("evt_0"), "T1/E1");
        assert!(is_transient_event_id("evt_18446744073709551615"), "T1/E1");
        assert!(!is_transient_event_id("evt_fnv1a64:abcdef"), "T2/E2");
        assert!(!is_transient_event_id("evt_"), "T3/E3");
        assert!(!is_transient_event_id("evt_12x"), "T3/E3");
    }

    #[test]
    fn session_and_thread_usage_share_the_frozen_pricing_decision_table() {
        // Cause/effect graph: C1 no frozen price snapshot exists; C2 one
        // attributed cumulative request exists under the creation snapshot; C3
        // a second request advances those cumulative counters; C4 the cap is
        // raised or removed while retaining the creation snapshot; C5 the same
        // cumulative prefix is projected again warm or cold; C6 token totals
        // lack served-model attribution. Effects: E1 preserve every counter and
        // omit optional cost; E2 price the first prefix as 14; E3 price the
        // cumulative second prefix as 28; E4 cap changes select no new price
        // authority; E5 replay is a pure stable value; E6 fail closed instead of
        // retaining a stale cost. The existing Thread-presence constraint Z1
        // keeps an all-zero Thread projection absent.
        //
        // | Rule | Snapshot | Cumulative prefix | Cap state | Replay | Attribution | Effects |
        // |---|---|---|---|---|---|---|
        // | R1 | absent | first | n/a | no | valid | E1 |
        // | R2 | creation | first | original | no | valid | E2 |
        // | R3 | creation | second | raised/removed | no | valid | E3,E4 |
        // | R4 | creation | second | raised/removed | warm/cold | valid | E3,E5 |
        // | R5 | creation | any nonzero | any | any | missing | E6 |
        assert!(
            session_thread_usage_value(SessionUsage::default(), None)
                .unwrap()
                .is_none(),
            "Z1"
        );
        let first = SessionUsage {
            input_tokens: 8,
            output_tokens: 4,
            cache_read_tokens: 1,
            cache_creation_tokens: 1,
            active_seconds: 2,
            web_fetch_requests: 1,
            web_search_requests: 4,
            by_model: std::collections::BTreeMap::from([(
                "served".into(),
                awaken_session_contract::SessionModelUsage {
                    input_tokens: 8,
                    output_tokens: 4,
                    cache_read_tokens: 1,
                    cache_creation_tokens: 1,
                },
            )]),
        };
        let unpriced = session_usage_value(first.clone(), None).unwrap();
        assert_eq!(unpriced.input_tokens, Some(8), "R1/E1");
        assert_eq!(unpriced.output_tokens, Some(4), "R1/E1");
        assert_eq!(unpriced.cache_read_input_tokens, Some(1), "R1/E1");
        assert_eq!(unpriced.active_seconds, Some(2), "R1/E1");
        assert_eq!(
            unpriced
                .cache_creation
                .as_ref()
                .and_then(|usage| usage.ephemeral_5m_input_tokens),
            Some(1),
            "R1/E1"
        );
        assert_eq!(unpriced.list_cost, None, "R1/E1");

        let snapshot = awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "prices-1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: std::collections::BTreeMap::from([(
                "served".into(),
                awaken_session_contract::ManagedTokenListRates {
                    input_micros_per_million: 10_000_000_000,
                    output_micros_per_million: 10_000_000_000,
                    cache_read_micros_per_million: 10_000_000_000,
                    cache_creation_micros_per_million: 10_000_000_000,
                },
            )]),
            runtime_rates: awaken_session_contract::ManagedRuntimeListRates::default(),
            fingerprint: "prices-fingerprint".into(),
        };
        let priced_session = session_usage_value(first.clone(), Some(&snapshot)).unwrap();
        assert_eq!(
            priced_session
                .list_cost
                .as_ref()
                .map(|cost| cost.amount.as_str()),
            Some("14"),
            "R2/E2"
        );
        let priced_thread = session_thread_usage_value(first, Some(&snapshot))
            .unwrap()
            .expect("R2 Thread accounting is visible");
        assert_eq!(
            priced_thread
                .list_cost
                .as_ref()
                .map(|cost| cost.amount.as_str()),
            Some("14"),
            "R2/E2 shared pricing helper"
        );

        let second = SessionUsage {
            input_tokens: 16,
            output_tokens: 8,
            cache_read_tokens: 2,
            cache_creation_tokens: 2,
            active_seconds: 4,
            web_fetch_requests: 2,
            web_search_requests: 8,
            by_model: std::collections::BTreeMap::from([(
                "served".into(),
                awaken_session_contract::SessionModelUsage {
                    input_tokens: 16,
                    output_tokens: 8,
                    cache_read_tokens: 2,
                    cache_creation_tokens: 2,
                },
            )]),
        };
        let mut raised = awaken_session_contract::SessionBudgetState::active(1, snapshot);
        let awaken_session_contract::SessionBudgetState::Active {
            max_list_cost_minor,
            ..
        } = &mut raised
        else {
            unreachable!()
        };
        *max_list_cost_minor = 100;
        let raised_projection =
            session_usage_value(second.clone(), raised.price_snapshot()).unwrap();
        assert_eq!(
            raised_projection
                .list_cost
                .as_ref()
                .map(|cost| cost.amount.as_str()),
            Some("28"),
            "R3/E3-E4 raised cap retains creation prices"
        );
        let removed = match raised {
            awaken_session_contract::SessionBudgetState::Active {
                consumed_numerator,
                usage_cursor,
                snapshot,
                reach_transitions,
                ..
            } => awaken_session_contract::SessionBudgetState::Removed {
                consumed_numerator,
                usage_cursor,
                snapshot,
                reach_transitions,
            },
            _ => unreachable!(),
        };
        let removed_projection =
            session_usage_value(second.clone(), removed.price_snapshot()).unwrap();
        assert_eq!(
            removed_projection
                .list_cost
                .as_ref()
                .map(|cost| cost.amount.as_str()),
            Some("28"),
            "R3/E3-E4 removed cap retains creation prices"
        );
        assert_eq!(
            serde_json::to_value(session_usage_value(second, removed.price_snapshot()).unwrap())
                .unwrap(),
            serde_json::to_value(removed_projection).unwrap(),
            "R4/E3,E5"
        );

        assert!(
            session_usage_value(
                SessionUsage {
                    input_tokens: 1,
                    ..Default::default()
                },
                removed.price_snapshot(),
            )
            .is_err(),
            "R5/E6"
        );
    }
}
