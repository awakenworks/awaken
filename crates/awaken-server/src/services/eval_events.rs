use awaken_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventScope, EventStoreError,
    EventVisibility, EventWriter,
};
use awaken_eval::EvalRun;
use serde_json::{Value, json};

use crate::app::EvalRoutesState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvalRunStartedEvent {
    pub eval_run_id: String,
    pub dataset_id: String,
    pub dataset_revision: u64,
    pub mode: &'static str,
    pub planned_item_count: usize,
    pub started_at_secs: u64,
}

pub(crate) async fn record_eval_run_started(state: &EvalRoutesState, event: EvalRunStartedEvent) {
    let Some(writer) = state
        .events
        .as_ref()
        .map(|events| events.event_store.clone())
    else {
        return;
    };
    if let Err(error) = append_eval_event(
        writer.as_ref(),
        "EvalRunStarted",
        &event.eval_run_id,
        json!({
            "eval_run_id": event.eval_run_id.as_str(),
            "dataset_id": event.dataset_id.as_str(),
            "dataset_revision": event.dataset_revision,
            "mode": event.mode,
            "planned_item_count": event.planned_item_count,
            "started_at_secs": event.started_at_secs,
        }),
    )
    .await
    {
        tracing::error!(error = %error, eval_run_id = %event.eval_run_id, "failed to record eval run started event");
    }
}

pub(crate) async fn record_eval_run_completed(
    state: &EvalRoutesState,
    run: &EvalRun,
    mode: &'static str,
    persisted: bool,
) {
    let Some(writer) = state
        .events
        .as_ref()
        .map(|events| events.event_store.clone())
    else {
        return;
    };
    if let Err(error) = append_eval_event(
        writer.as_ref(),
        "EvalRunCompleted",
        &run.id,
        completed_payload(run, mode, persisted),
    )
    .await
    {
        tracing::error!(error = %error, eval_run_id = %run.id, "failed to record eval run completed event");
    }
}

async fn append_eval_event(
    writer: &dyn EventWriter,
    event_kind: &'static str,
    eval_run_id: &str,
    payload: Value,
) -> Result<(), EventStoreError> {
    let mut draft = CanonicalEventDraft::new(
        vec![EventScope::run(eval_run_id.to_string())],
        CanonicalEventKind::new(event_kind)?,
        payload,
        "eval",
    )?;
    draft.visibility = EventVisibility::Internal;
    draft.correlation_id = Some(eval_run_id.to_string());
    writer
        .append(
            draft,
            AppendOptions {
                writer_id: Some("eval".to_string()),
                idempotency_key: Some(format!("{event_kind}/{eval_run_id}")),
                expected_prior_cursors: Default::default(),
            },
        )
        .await?;
    Ok(())
}

fn completed_payload(run: &EvalRun, mode: &'static str, persisted: bool) -> Value {
    let passed_count = run.items.iter().filter(|item| item.report.passed).count();
    let failed_count = run.items.len().saturating_sub(passed_count);
    let total_cost_usd = run
        .items
        .iter()
        .filter_map(|item| item.report.cost_usd)
        .sum::<f64>();
    json!({
        "eval_run_id": run.id,
        "dataset_id": run.dataset_id,
        "dataset_revision": run.dataset_revision,
        "mode": mode,
        "item_count": run.items.len(),
        "passed_count": passed_count,
        "failed_count": failed_count,
        "total_cost_usd": total_cost_usd,
        "started_at_secs": run.started_at_secs,
        "ended_at_secs": run.ended_at_secs,
        "persisted": persisted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::event_store::{EventReader, EventScope};
    use awaken_eval::{EvalRunItem, MatrixCell, ReplayReport};
    use awaken_stores::InMemoryEventStore;

    fn report(passed: bool, cost_usd: Option<f64>) -> ReplayReport {
        ReplayReport {
            fixture_id: "fixture-1".to_string(),
            passed,
            failures: Vec::new(),
            final_text: "ok".to_string(),
            inference_count: 1,
            tool_count: 0,
            tool_failures: 0,
            total_input_tokens: 10,
            total_output_tokens: 5,
            total_tokens: 15,
            session_duration_ms: 20,
            elapsed_ms: 20,
            tool_calls_by_agent: Vec::new(),
            error_type: None,
            inference_error_count: 0,
            runtime_failure: None,
            cost_usd,
            revision_count: 0,
            judge_score: None,
            judge_reasoning: None,
        }
    }

    fn eval_run() -> EvalRun {
        EvalRun {
            id: "eval_1".to_string(),
            dataset_id: "dataset-a".to_string(),
            dataset_revision: 7,
            items: vec![
                EvalRunItem {
                    fixture_id: "fixture-1".to_string(),
                    cell: Some(MatrixCell {
                        model_id: Some("model-a".to_string()),
                    }),
                    report: report(true, Some(0.25)),
                    trace_run_id: None,
                    sample_index: None,
                },
                EvalRunItem {
                    fixture_id: "fixture-2".to_string(),
                    cell: None,
                    report: report(false, Some(0.5)),
                    trace_run_id: None,
                    sample_index: None,
                },
            ],
            started_at_secs: 10,
            ended_at_secs: 12,
        }
    }

    #[tokio::test]
    async fn append_eval_events_are_scoped_to_eval_run() {
        let store = InMemoryEventStore::new();
        append_eval_event(
            &store,
            "EvalRunStarted",
            "eval_1",
            json!({"eval_run_id":"eval_1","planned_item_count":2}),
        )
        .await
        .unwrap();
        append_eval_event(
            &store,
            "EvalRunCompleted",
            "eval_1",
            completed_payload(&eval_run(), "dataset_live_matrix", true),
        )
        .await
        .unwrap();

        let page = store
            .list(EventScope::run("eval_1"), None, 10)
            .await
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0].event_kind.as_str(), "EvalRunStarted");
        assert_eq!(page.events[1].event_kind.as_str(), "EvalRunCompleted");
        assert_eq!(page.events[1].payload["passed_count"], 1);
        assert_eq!(page.events[1].payload["failed_count"], 1);
        assert_eq!(page.events[1].payload["total_cost_usd"], 0.75);
        assert_eq!(page.events[1].visibility, EventVisibility::Internal);
    }
}
