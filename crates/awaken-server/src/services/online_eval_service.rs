//! `POST /v1/eval/online` — ad-hoc evaluation against real provider
//! executors (ADR-0032 extension).
//!
//! Differs from `POST /v1/eval/runs` in three ways:
//!
//! 1. **No dataset needed**: the request body carries a single
//!    `user_input` directly. Operators can test "this prompt × these
//!    models" without persisting a regression suite first.
//! 2. **Live execution by default**: every cell drives a real provider
//!    executor resolved from `model_bindings/{model_id}` →
//!    `providers/{provider_id}`. No scripted upstream.
//! 3. **Matrix shape**: `models: Vec<String>` is the model axis. The
//!    response carries `items.len() == models.len()`, one per cell.
//!
//! Persistence is opt-in via `persist=true` (default `true` — exploration
//! tends to be retroactively interesting). Persisted runs land in the
//! shared `EvalRunStore` with the `dataset_id` set to a sentinel
//! `_adhoc` value so they're filterable separately from regression runs.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_contract::agent_spec_patch::AgentSpecPatch;
use awaken_eval::{
    EvalRun, EvalRunItem, LlmExecutorJudge, MatrixCell, ReplayReport, RuntimeReplayer,
    expand_cells, mint_run_id, replay_all,
};
use awaken_eval::{Expectation, Fixture, MockResponse};
use awaken_ext_observability::trace_store::TraceStoreSink;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::error::ApiError;
use crate::services::eval_common::resolve_live_executor;

/// Sentinel dataset id for ad-hoc online eval runs. Lets `/v1/eval/runs`
/// list filters distinguish "regression-suite vs my-exploration"
/// without a separate persistence backend.
pub(crate) const ADHOC_DATASET_ID: &str = "_adhoc";

// Concurrency / cell / sample / revision caps all live on
// `ServerConfig::eval_limits` — see `crate::app::EvalLimits`. The
// per-handler reads sit at the top of `start_online_eval`.

const DEFAULT_MAX_WALLTIME_SECS: u64 = 60;
const DEFAULT_MAX_TOTAL_TOKENS: u32 = 10_000;

// ── Wire types ────────────────────────────────────────────────────────────

/// Request body for `POST /v1/eval/online`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnlineEvalRequest {
    /// Prompt to evaluate. Single text input for v1; multi-modal
    /// (`ContentBlock[]`) is a forward-compat enhancement.
    pub user_input: String,
    /// Models to evaluate against. Each becomes one matrix cell; each
    /// runs in parallel up to `MAX_CONCURRENT_CELLS`. Must be non-empty.
    pub models: Vec<String>,
    /// Registered agent whose `system_prompt` / `allowed_tools` /
    /// sampling params should be used as the base for every cell.
    /// Without this, every cell runs against a synthetic stub agent —
    /// useful for "prompt sketching" but not for evaluating a real
    /// agent's behaviour. `agent_overrides` (below) merges as a patch
    /// on top.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional agent overrides — system prompt, allowed tools,
    /// sampling params, etc. Applied as a patch on top of the spec
    /// pulled for `agent_id`, or on top of the synthetic stub when
    /// `agent_id` is unset. `model_id` inside the patch is ignored
    /// (the cell's `model_id` is what's tested).
    #[serde(default)]
    pub agent_overrides: Option<AgentSpecPatch>,
    /// Optional graders. When absent, the run records final_text +
    /// token counts but doesn't pass/fail anything.
    #[serde(default)]
    pub expectations: Option<Expectation>,
    /// Persist the run to `EvalRunStore` for later inspection / diff.
    /// Default true — exploration is retroactively interesting.
    #[serde(default = "default_persist")]
    pub persist: bool,
    /// Per-cell walltime cap. Enforced as a single deadline shared
    /// across two phases: (1) replay (initial inference + any dialogue
    /// continuation + revise loop) and (2) scoring + judge invocation.
    /// Both `tokio::time::timeout_at` against the same `Instant +
    /// max_walltime_secs`, so a stuck judge eats into the same budget
    /// the model already consumed. On expiry the in-flight provider
    /// call is dropped; the cell's `EvalRunItem` reports
    /// `ReplayRuntimeFailure::RuntimeError`. When the timeout fires
    /// AFTER replay completed, the real `final_text` / token usage /
    /// trace_run_id are preserved — only the scoring/judge phase is
    /// reported as the failure reason. Prevents a stuck provider or
    /// judge from holding the HTTP connection past ingress timeouts.
    #[serde(default = "default_walltime")]
    pub max_walltime_secs: u64,
    /// Per-cell token budget (post-hoc enforced — see
    /// `RuntimeReplayer::with_max_total_tokens` docstring).
    #[serde(default = "default_token_budget")]
    pub max_total_tokens: u32,
    /// Per-cell flakiness sample count. Each cell runs `samples` times
    /// so non-determinism becomes visible as a pass_rate / latency
    /// distribution instead of a 1-shot guess. Default `None` =
    /// single sample (current behaviour). Total units
    /// (cells × samples) must stay under [`MAX_CELLS_PER_SYNC_ONLINE`].
    #[serde(default)]
    pub samples: Option<u32>,
    /// Optional LLM-as-judge config. When set and `expectations.min_judge_score`
    /// is also set, each cell's outcome is graded by `judge.model_id`; a score
    /// below threshold appends `Failure::JudgeBelowThreshold`.
    #[serde(default)]
    pub judge: Option<crate::services::eval_run_service::JudgeRequest>,
}

fn default_persist() -> bool {
    true
}
fn default_walltime() -> u64 {
    DEFAULT_MAX_WALLTIME_SECS
}
fn default_token_budget() -> u32 {
    DEFAULT_MAX_TOTAL_TOKENS
}

/// Response for `POST /v1/eval/online`. Same shape as `EvalRun` whether
/// persisted or ephemeral; `persisted=false` means the operator must
/// hold onto `run.id` themselves to fetch it later (and it won't be).
#[derive(Debug, Serialize)]
pub struct OnlineEvalResponse {
    pub run: EvalRun,
    /// `true` when the run was written to `EvalRunStore`. When `false`,
    /// `GET /v1/eval/runs/:id` will return 404.
    pub persisted: bool,
}

// ── Handler ──────────────────────────────────────────────────────────────

/// `POST /v1/eval/online` — drive `user_input` against every model in
/// `models`, score the results, return (and optionally persist) an
/// `EvalRun` whose items are one per cell.
#[tracing::instrument(skip_all, fields(models = ?body.models))]
pub async fn start_online_eval(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OnlineEvalRequest>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;

    // Fail fast on persist+no-store BEFORE any model resolution or
    // provider call. Otherwise we'd burn tokens on a request that the
    // persistence layer is already known to reject — bad UX, real $.
    if body.persist && state.eval_run_store().is_none() {
        return Err(ApiError::ServiceUnavailable(
            "EvalRunStore is required when persist=true".into(),
        ));
    }
    let limits = state.config.eval_limits.clone();
    if body.models.is_empty() {
        return Err(ApiError::BadRequest(
            "models must contain at least one model id".into(),
        ));
    }
    crate::services::eval_cell::validate_unique_models(&body.models)?;
    if body.max_walltime_secs == 0 {
        return Err(ApiError::BadRequest(
            "max_walltime_secs must be >= 1 (0 would time out every cell immediately)".into(),
        ));
    }
    let cells = expand_cells(&body.models);
    if body.samples == Some(0) {
        return Err(ApiError::BadRequest(
            "samples must be >= 1 (omit the field for a single sample)".into(),
        ));
    }
    let samples = body.samples.unwrap_or(1).max(1);
    if samples > limits.max_samples_per_cell {
        return Err(ApiError::BadRequest(format!(
            "samples={samples} exceeds cap {}",
            limits.max_samples_per_cell
        )));
    }
    let total_units = cells.len() * samples as usize;
    if total_units > limits.max_cells_per_sync_online {
        return Err(ApiError::BadRequest(format!(
            "{total_units} units (cells × samples) exceed sync online cap {}; \
             split or persist as a dataset and use /v1/eval/runs",
            limits.max_cells_per_sync_online,
        )));
    }
    // Validate judge.revise_max_retries cap up-front so a typo fails
    // before any registry lookup burns latency.
    if let Some(jr) = body.judge.as_ref()
        && let Some(n) = jr.revise_max_retries
        && n > limits.max_judge_revisions
    {
        return Err(ApiError::BadRequest(format!(
            "revise_max_retries={n} exceeds cap {}",
            limits.max_judge_revisions
        )));
    }

    // Resolve the registered agent ONCE up front (cheap config-store
    // lookup; runs before any provider call). Without `agent_id`, every
    // cell falls back to the synthetic stub agent (the historical
    // behaviour — useful for "is this prompt under the right model?"
    // probing).
    let agent_base = match &body.agent_id {
        Some(id) => Some(crate::services::eval_common::resolve_agent_spec(&state, id).await?),
        None => None,
    };

    // Pre-resolve every model so any 404 surfaces before we start
    // burning provider tokens. Carries the binding spec forward so we
    // can compute cost_usd post-replay.
    let mut resolved: Vec<(
        MatrixCell,
        Arc<dyn awaken_contract::contract::executor::LlmExecutor>,
        String,
        awaken_contract::registry_spec::ModelBindingSpec,
    )> = Vec::with_capacity(cells.len());
    for cell in cells {
        let model_id = cell
            .model_id
            .as_deref()
            .expect("expand_cells always sets model_id when models is non-empty");
        let r = resolve_live_executor(&state, model_id).await?;
        resolved.push((cell, r.executor, r.upstream_model, r.binding));
    }

    // Resolve the judge executor (if configured) before any cell runs
    // so a bad judge model fails fast.
    let judge = if let Some(ref jr) = body.judge {
        // (cap already validated above; duplicate check here was
        // defensive and now unnecessary)
        let resolved = resolve_live_executor(&state, &jr.model_id).await?;
        Some(crate::services::eval_run_service::JudgeContext {
            judge: LlmExecutorJudge::new(resolved.executor, resolved.upstream_model),
            rubric: jr.rubric.clone(),
            revise_max_retries: jr.revise_max_retries,
        })
    } else {
        None
    };

    // Build the ad-hoc fixture once — the same user_input + expectations
    // feeds every cell.
    let fixture = Fixture {
        id: ADHOC_DATASET_ID.into(),
        description: None,
        user_input: body.user_input.clone(),
        provider_script: vec![],
        source_run_id: None,
        source_model_id: None,
        allow_unused_provider_script: false,
        mock_response: MockResponse::default(),
        expect: body.expectations.clone().unwrap_or_default(),
        continued_turns: vec![],
    };

    // Capture started_at_secs BEFORE the per-cell execution loop so the
    // recorded time matches when work actually began.
    let started_at_secs = epoch_secs_now();
    // Run cells × samples in parallel with bounded concurrency.
    let trace_sink = state.trace_store().map(|store| {
        Arc::new(TraceStoreSink::new(store)) as Arc<dyn awaken_ext_observability::MetricsSink>
    });
    let semaphore = Arc::new(tokio::sync::Semaphore::new(
        limits.max_concurrent_online_cells,
    ));
    let mut handles = Vec::with_capacity(total_units);
    // Same convention as the dataset path: only emit `sample_index`
    // when the caller actually asked for more than one sample, so
    // single-sample runs keep their original on-disk shape.
    let emit_sample_index = samples > 1;
    for (cell, executor, upstream_model, binding) in resolved {
        for sample in 0..samples {
            let fixture = fixture.clone();
            let executor = executor.clone();
            let upstream_model = upstream_model.clone();
            let binding = binding.clone();
            let overrides = body.agent_overrides.clone();
            let base = agent_base.clone();
            let max_tokens = body.max_total_tokens;
            let walltime = std::time::Duration::from_secs(body.max_walltime_secs);
            let trace_sink = trace_sink.clone();
            let cell = cell.clone();
            let fixture_id = fixture.id.clone();
            let judge_for_task = judge.clone();
            let revise_for_task = crate::services::eval_run_service::revise_tuple_for(
                judge.as_ref(),
                &fixture.expect,
            );
            let permit = semaphore.clone().acquire_owned().await.expect("semaphore");
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                let mut builder = RuntimeReplayer::new()
                    .with_live_executor(executor, upstream_model)
                    .with_max_total_tokens(max_tokens);
                if let Some(b) = base {
                    builder = builder.with_agent_base(b);
                }
                let replayer = crate::services::eval_run_service::apply_cell_decorators(
                    builder,
                    overrides,
                    trace_sink,
                    revise_for_task,
                );
                // Per-cell wall-clock cap covers replay + scoring + judge
                // as a SINGLE deadline split across two `timeout_at`
                // calls — a scoring/judge timeout AFTER replay completed
                // routes to `cell_error_outcome(real_outcome, …)` so the
                // model's actual `final_text` / tokens / trace are
                // preserved, rather than collapsing into an empty
                // synthetic timeout outcome that would fabricate phantom
                // `AnswerMissingPhrase` failures.
                let deadline = tokio::time::Instant::now() + walltime;
                let walltime_secs = walltime.as_secs();
                let outcome = match tokio::time::timeout_at(deadline, async {
                    let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
                    outcomes
                        .into_iter()
                        .next()
                        .expect("one fixture → one outcome")
                })
                .await
                {
                    Ok(o) => o,
                    Err(_) => {
                        let (o, f) = crate::services::eval_cell::cell_timeout_outcome(
                            fixture_id,
                            walltime_secs,
                            &fixture.expect,
                        );
                        return Ok::<_, ApiError>((cell, sample, o, f, binding));
                    }
                };
                let (outcome, failures) = match tokio::time::timeout_at(
                    deadline,
                    crate::services::eval_run_service::score_outcome(
                        &outcome,
                        &fixture,
                        judge_for_task.as_ref(),
                    ),
                )
                .await
                {
                    Ok(Ok(failures)) => (outcome, failures),
                    Ok(Err(err)) => crate::services::eval_cell::cell_error_outcome(
                        outcome,
                        format!("scoring failed: {err}"),
                        &fixture.expect,
                    ),
                    Err(_) => crate::services::eval_cell::cell_error_outcome(
                        outcome,
                        format!(
                            "scoring timed out after {walltime_secs}s wall-clock (replay completed)"
                        ),
                        &fixture.expect,
                    ),
                };
                Ok::<_, ApiError>((cell, sample, outcome, failures, binding))
            }));
        }
    }

    let mut items: Vec<EvalRunItem> = Vec::with_capacity(handles.len());
    for handle in handles {
        let task_result = handle
            .await
            .map_err(|err| ApiError::Internal(format!("online cell task panicked: {err}")))?;
        let (cell, sample, outcome, failures, binding) = task_result?;
        let mut report = ReplayReport::from_outcome(&outcome, failures);
        report.cost_usd = crate::services::eval_cell::cost_usd_for(&report, &binding);
        items.push(EvalRunItem {
            fixture_id: fixture.id.clone(),
            cell: Some(cell),
            report,
            trace_run_id: outcome.trace_run_id().map(str::to_string),
            sample_index: if emit_sample_index {
                Some(sample)
            } else {
                None
            },
        });
    }

    let run = EvalRun {
        id: mint_run_id(),
        dataset_id: ADHOC_DATASET_ID.into(),
        dataset_revision: 0,
        items,
        started_at_secs,
        ended_at_secs: epoch_secs_now(),
    };

    let persisted = if body.persist {
        if let Some(store) = state.eval_run_store() {
            store
                .write(&run)
                .map_err(super::eval_run_service::map_eval_run_store_error)?;
            true
        } else {
            // EvalRunStore not configured — surface explicitly rather
            // than silently lying about persistence.
            return Err(ApiError::ServiceUnavailable(
                "persist=true requested but EvalRunStore is not configured".into(),
            ));
        }
    } else {
        false
    };

    Ok(Json(OnlineEvalResponse { run, persisted }).into_response())
}

fn epoch_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
