//! Runtime-backed [`Replayer`] that builds a real [`AgentRuntime`] with a
//! per-fixture [`ScriptedLlmExecutor`] and harvests the resulting
//! observability spans into a [`ReplayOutcome`].
//!
//! `RuntimeReplayer` exercises the full agent loop and lets
//! `awaken-ext-observability` record real spans. For fixtures driven by
//! an explicit `provider_script`, every token count and stop reason in
//! the resulting `AgentMetrics` comes straight from the script. Legacy
//! `mock_response: { kind: "text" }` fixtures still load through
//! [`Fixture::effective_script`], which seeds `TokenUsage` with a
//! `chars / 4` estimate to preserve the original `max_tokens_total`
//! semantics until those fixtures migrate.
//!
//! ## Determinism contract
//!
//! Eval replays must be reproducible across CI hosts. The replayer
//! therefore:
//!
//! - **Disables LLM retries** (`max_retries = 0`). A scripted `Error`
//!   event is consumed exactly once, so a `rate_limit` fixture cannot be
//!   silently turned into "first attempt errors, retries exhaust the
//!   script, runtime reports a different failure".
//! - **Asserts the `provider_script` is fully consumed** after the run,
//!   unless the fixture opts in to `allow_unused_provider_script`. This
//!   catches scripts where the runtime stopped early — e.g. a tool round
//!   that never fired or an expected retry that never happened.
//! - **Pins `upstream_model`** to [`Fixture::source_model_id`] when set,
//!   binding the scripted provider to that model and guarding every
//!   `InferenceRequest` through the executor.
//! - **Surfaces `error_type`** of the first scripted error event into
//!   [`ReplayOutcome::error_type`]. Without this, the runtime's
//!   `AgentLoopError::InferenceFailed(String)` would flatten the error
//!   variant and `05_error_path`-style fixtures would silently pass on
//!   "final text doesn't contain success".
//!
//! This is the eval framework's single source of truth for "what would
//! happen in production" once ADR-0032 is wired end-to-end.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use awaken_contract::agent_spec_patch::{AgentSpecPatch, merge_agent_spec};
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::message::Message;
use awaken_contract::registry_spec::AgentSpec;
use awaken_ext_observability::{CompositeSink, InMemorySink, MetricsSink, ObservabilityPlugin};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::engine::{LlmRetryPolicy, RetryConfigKey, ScriptedLlmExecutor};
use awaken_runtime::registry::traits::ModelBinding;
use awaken_runtime::{AgentRuntime, RunRequest};
use awaken_stores::memory::InMemoryStore;

use crate::fixture::Fixture;
use crate::outcome::{ReplayOutcome, ReplayRuntimeFailure};
use crate::replay::Replayer;

/// Identifier the scripted provider registers under.
const SCRIPTED_PROVIDER_ID: &str = "scripted";
/// Identifier the live provider registers under.
const LIVE_PROVIDER_ID: &str = "live";
/// Identifier for the model binding the agent spec points at.
const SCRIPTED_MODEL_ID: &str = "scripted-model";
/// Identifier for the live model binding the agent spec points at when
/// `ReplayMode::Live`. The caller-supplied `upstream_model` is bound to
/// the live provider under this id; agent overrides that try to set a
/// different `model_id` are ignored (the live executor is the model under
/// test, by definition).
const LIVE_MODEL_ID: &str = "live-model";
/// Default upstream model name used when the fixture does not pin
/// [`Fixture::source_model_id`].
const SCRIPTED_UPSTREAM_MODEL_DEFAULT: &str = "scripted";
/// Identifier of the synthetic agent driven by the replay.
const DEFAULT_AGENT_ID: &str = "default";
/// Static system prompt the synthetic agent uses.
const DEFAULT_SYSTEM_PROMPT: &str = "You are a test assistant.";

/// How the replay sources its LLM responses.
///
/// `Scripted` is the original (and default) mode: deterministic replay
/// against the fixture's `provider_script`, used for CI smoke tests.
/// `Live` swaps the scripted executor for a real provider — the LLM
/// actually runs — used for "does our agent still work against this
/// model" regression and ad-hoc online evaluation.
#[derive(Default)]
pub enum ReplayMode {
    #[default]
    Scripted,
    Live {
        /// Real provider executor, typically built from a `ProviderSpec`
        /// in the server's `ConfigRuntimeManager` and passed in here.
        executor: Arc<dyn LlmExecutor>,
        /// Upstream model id the executor should pass to the provider.
        /// Bound under `LIVE_MODEL_ID` in the synthetic registry; the
        /// agent's `model_id` is forced to `LIVE_MODEL_ID` even if
        /// `agent_overrides.model_id` was supplied (the live model is
        /// what's under test).
        upstream_model: String,
        /// Optional agent-spec overrides applied via [`merge_agent_spec`].
        /// `model_id` in the patch is ignored (see above); everything
        /// else (system_prompt, allowed_tools, temperature, etc.) merges
        /// onto the default replay agent. Boxed so the variant's stack
        /// size stays small (AgentSpecPatch is large; the empty
        /// `Scripted` variant would otherwise drag the whole enum).
        agent_overrides: Option<Box<AgentSpecPatch>>,
        /// Post-hoc token budget. After replay completes, if
        /// `outcome.total_tokens() > max`, a
        /// [`ReplayRuntimeFailure::RuntimeError`] is recorded with a
        /// `"token budget exceeded"` message. Real-time cancellation
        /// requires a cancellation token plumbed through the runtime —
        /// that's a follow-up; the soft cap catches "this fixture cost
        /// $X" without aborting expensive in-flight inference.
        max_total_tokens: Option<u32>,
        /// Optional reprocess-on-judge-fail loop. Mirrors Anthropic
        /// Managed Agents' Outcomes feedback path: after the initial
        /// run, judge the result; if the score is below threshold and
        /// retries remain, append a synthesised user message ("your
        /// answer was X, here's why it failed: …, please revise") on
        /// the same thread and re-run the agent. Stops on the first
        /// passing score or when retries exhaust.
        revise: Option<ReviseConfig>,
    },
}

/// Configuration for the reprocess-on-judge-fail loop (Live mode only).
#[derive(Clone)]
pub struct ReviseConfig {
    pub judge: Arc<dyn crate::judge::Judge>,
    pub rubric: Option<String>,
    pub threshold: f32,
    pub max_retries: u32,
}

/// Replayer that drives a real [`AgentRuntime`] using the fixture's
/// `provider_script` (or the legacy `mock_response` shim).
pub struct RuntimeReplayer {
    max_rounds_floor: usize,
    /// Optional sink that gets a copy of every metrics event the
    /// observability plugin records. Set by callers (e.g. the server's
    /// eval-run service) that want replay spans to land in a shared
    /// [`TraceStore`] alongside production traces.
    tee_sink: Option<Arc<dyn MetricsSink>>,
    /// Optional base agent spec for Live mode. When `None`, the live
    /// replayer synthesises a stub agent with `DEFAULT_SYSTEM_PROMPT`.
    /// When set (typically by the server pulling the registered agent
    /// for `body.agent_id`), this is used as the base before
    /// `agent_overrides` merges on top — so the eval exercises the
    /// agent's real `system_prompt` / tool list / sampling params, not
    /// a synthetic stub.
    agent_base: Option<Box<AgentSpec>>,
    /// Replay mode — Scripted (default) or Live.
    mode: ReplayMode,
}

impl RuntimeReplayer {
    pub fn new() -> Self {
        Self {
            max_rounds_floor: 4,
            tee_sink: None,
            agent_base: None,
            mode: ReplayMode::default(),
        }
    }

    /// Override the minimum `max_rounds` applied to the synthetic
    /// agent spec. The effective value is `max(floor, script.len() + 1)`
    /// so scripts that emit several tool calls before a final response
    /// don't get clipped by the loop runner.
    #[must_use]
    pub fn with_max_rounds_floor(mut self, floor: usize) -> Self {
        self.max_rounds_floor = floor;
        self
    }

    /// Tee every metrics event the replay records into `sink`. The
    /// in-memory aggregation that feeds `ReplayOutcome.metrics` is
    /// preserved — the tee is additive. Typical caller: the server's
    /// eval-run service wires a `TraceStoreSink` so the admin UI can
    /// pivot from an `EvalRunItem.trace_run_id` to the full trace.
    #[must_use]
    pub fn with_tee_sink(mut self, sink: Arc<dyn MetricsSink>) -> Self {
        self.tee_sink = Some(sink);
        self
    }

    /// Switch the replayer into Live mode: instead of replaying the
    /// fixture's `provider_script`, drive the supplied `executor` (a
    /// real provider) against `upstream_model`. The fixture's
    /// `provider_script` is ignored. `agent_overrides` (optional) is
    /// merged onto the default replay agent so callers can pin a
    /// system prompt, allowed tools, or sampling params; `model_id`
    /// inside the patch is silently overridden by `LIVE_MODEL_ID`.
    #[must_use]
    pub fn with_live_executor(
        mut self,
        executor: Arc<dyn LlmExecutor>,
        upstream_model: impl Into<String>,
    ) -> Self {
        self.mode = ReplayMode::Live {
            executor,
            upstream_model: upstream_model.into(),
            agent_overrides: None,
            max_total_tokens: None,
            revise: None,
        };
        self
    }

    /// Enable the reprocess-on-judge-fail loop. After the initial Live
    /// replay, the judge scores the outcome; if score < `threshold` and
    /// retries remain, a synthesised "revise this" user message is
    /// appended to the same thread and the agent re-runs. Stops when
    /// score ≥ threshold or `max_retries` are exhausted.
    ///
    /// No-op on Scripted mode (scripted scripts don't tolerate
    /// extra inference calls; reprocessing semantically requires Live).
    #[must_use]
    pub fn with_revise_on_judge_fail(
        mut self,
        judge: Arc<dyn crate::judge::Judge>,
        rubric: Option<String>,
        threshold: f32,
        max_retries: u32,
    ) -> Self {
        if let ReplayMode::Live { revise, .. } = &mut self.mode {
            *revise = Some(ReviseConfig {
                judge,
                rubric,
                threshold,
                max_retries,
            });
        }
        self
    }

    /// Supply the base [`AgentSpec`] for Live mode. Typically the
    /// server pulls the registered spec for `body.agent_id` and passes
    /// it here so the eval runs against the agent's real
    /// `system_prompt` / tool list / sampling params. Without this
    /// call, Live mode falls back to a synthetic stub with
    /// [`DEFAULT_SYSTEM_PROMPT`].
    ///
    /// Safety: `id`, `model_id`, and `plugin_ids` are force-pinned
    /// post-merge inside `replay_live` so the eval cannot route to a
    /// real agent id, mismatch the live model, or trigger
    /// side-effectful plugins (mcp_*, skills with HTTP, …) regardless
    /// of what the registered spec carries. The base influences
    /// `system_prompt`, `allowed_tools`, sampling, and other
    /// pure-input behaviour only.
    #[must_use]
    pub fn with_agent_base(mut self, base: AgentSpec) -> Self {
        self.agent_base = Some(Box::new(base));
        self
    }

    /// Apply an [`AgentSpecPatch`] to the agent spec used by Live mode.
    /// No-op on Scripted mode (scripted runs use a fixed minimal agent).
    /// Calling this before `with_live_executor` is a logic error and
    /// will be silently overwritten.
    #[must_use]
    pub fn with_agent_overrides(mut self, patch: AgentSpecPatch) -> Self {
        if let ReplayMode::Live {
            agent_overrides, ..
        } = &mut self.mode
        {
            *agent_overrides = Some(Box::new(patch));
        }
        self
    }

    /// Cap the cumulative token count for a Live replay. After the
    /// replay completes, if `outcome.total_tokens() > max`, the outcome
    /// is annotated with [`ReplayRuntimeFailure::RuntimeError`] so the
    /// scorer surfaces it as a failure. Real-time interruption is
    /// deferred — this is a post-hoc soft cap.
    #[must_use]
    pub fn with_max_total_tokens(mut self, max: u32) -> Self {
        if let ReplayMode::Live {
            max_total_tokens, ..
        } = &mut self.mode
        {
            *max_total_tokens = Some(max);
        }
        self
    }
}

impl Default for RuntimeReplayer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Replayer for RuntimeReplayer {
    async fn replay(&self, fixture: &Fixture) -> ReplayOutcome {
        match &self.mode {
            ReplayMode::Scripted => self.replay_scripted(fixture).await,
            ReplayMode::Live {
                executor,
                upstream_model,
                agent_overrides,
                max_total_tokens,
                revise,
            } => {
                // Box → owned AgentSpecPatch for the replay_live call;
                // the field stays boxed in the enum to keep the variant small.
                self.replay_live(
                    fixture,
                    executor.clone(),
                    upstream_model.clone(),
                    agent_overrides.as_deref().cloned(),
                    *max_total_tokens,
                    revise.clone(),
                )
                .await
            }
        }
    }
}

impl RuntimeReplayer {
    async fn replay_scripted(&self, fixture: &Fixture) -> ReplayOutcome {
        // Combined script across turn 0 + every continued turn. The
        // ScriptedLlmExecutor's pointer advances naturally as each turn's
        // agent loop pulls events, so concatenation is sufficient — no
        // mid-replay re-seeding required.
        let script = fixture.combined_script();
        let sink = InMemorySink::new();
        // When a tee sink is wired (typically by the server's eval-run
        // service to forward into a TraceStore), the observability
        // plugin gets a CompositeSink that broadcasts to both. Without
        // a tee, the bare InMemorySink keeps the runtime cheap.
        let plugin = match &self.tee_sink {
            Some(tee) => {
                let composite = CompositeSink::builder()
                    .with_sink(Arc::new(sink.clone()))
                    .with_sink(tee.clone())
                    .build();
                ObservabilityPlugin::new(composite).with_provider(SCRIPTED_PROVIDER_ID)
            }
            None => ObservabilityPlugin::new(sink.clone()).with_provider(SCRIPTED_PROVIDER_ID),
        };

        let store = Arc::new(InMemoryStore::new());
        let max_rounds = std::cmp::max(self.max_rounds_floor, script.len().saturating_add(1));

        let upstream_model = fixture
            .source_model_id
            .clone()
            .unwrap_or_else(|| SCRIPTED_UPSTREAM_MODEL_DEFAULT.to_string());

        // Pin the executor to the fixture's source model when one was
        // captured. The guard rejects mismatched `InferenceRequest`s with
        // `InvalidRequest` *without* consuming a scripted event.
        let executor = {
            let exec = ScriptedLlmExecutor::new(script);
            match &fixture.source_model_id {
                Some(model) => Arc::new(exec.with_expected_upstream_model(model.clone())),
                None => Arc::new(exec),
            }
        };

        // Disable LLM retries: a `rate_limit` scripted event is a single
        // explicit error, not an invitation for the runtime to retry into
        // a different failure mode. Anyone needing scripted retries must
        // express them as additional `Error` events.
        let agent_spec = AgentSpec {
            id: DEFAULT_AGENT_ID.into(),
            model_id: SCRIPTED_MODEL_ID.into(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_rounds,
            plugin_ids: vec!["observability".into()],
            ..Default::default()
        }
        .with_config::<RetryConfigKey>(LlmRetryPolicy::no_retry())
        .expect("LlmRetryPolicy serialises into AgentSpec.sections[\"retry\"]");

        let runtime: Arc<AgentRuntime> = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider(SCRIPTED_PROVIDER_ID, executor.clone())
                .with_model_binding(
                    SCRIPTED_MODEL_ID,
                    ModelBinding {
                        provider_id: SCRIPTED_PROVIDER_ID.into(),
                        upstream_model: upstream_model.clone(),
                    },
                )
                .with_thread_run_store(store.clone())
                .with_agent_spec(agent_spec)
                .with_plugin("observability", Arc::new(plugin))
                .build()
                .expect("scripted runtime builds"),
        );

        let thread_id = format!("eval-thread-{}", fixture.id);
        let inputs: Vec<&str> = std::iter::once(fixture.user_input.as_str())
            .chain(
                fixture
                    .continued_turns
                    .iter()
                    .map(|t| t.user_input.as_str()),
            )
            .collect();

        let start = Instant::now();
        let mut final_text = String::new();
        let mut last_error_msg: Option<String> = None;
        // Same-thread reuse: each successive run_to_completion loads the
        // prior turn's history from the in-memory store and appends the
        // new user input — see RunRequest::thread_id docstring. First
        // error short-circuits the dialogue; the surviving turns'
        // expected behaviour is undefined past an error anyway.
        for input in inputs {
            let request = RunRequest::new(thread_id.clone(), vec![Message::user(input)])
                .with_agent_id(DEFAULT_AGENT_ID);
            match runtime.run_to_completion(request).await {
                Ok(result) => final_text = result.response,
                Err(err) => {
                    final_text = String::new();
                    last_error_msg = Some(err.to_string());
                    break;
                }
            }
        }
        let elapsed = start.elapsed();

        let scripted_error = executor.first_error();
        let error_type = match &last_error_msg {
            None => None,
            // Prefer the *fixture-author-supplied* error_type captured by
            // the executor before the variant got flattened into
            // `AgentLoopError::InferenceFailed(String)`.
            Some(_) => scripted_error.as_ref().map(|(kind, _msg)| kind.clone()),
        };

        let runtime_failure = decide_runtime_failure(
            executor.exhausted_calls(),
            executor.remaining(),
            last_error_msg,
            scripted_error.is_some(),
            fixture.allow_unused_provider_script,
        );

        ReplayOutcome {
            fixture_id: fixture.id.clone(),
            final_text,
            metrics: sink.metrics(),
            elapsed,
            error_type,
            inference_error_count: executor.error_calls(),
            runtime_failure,
            revision_count: 0,
            judge_score: None,
            judge_reasoning: None,
        }
    }

    /// Drive the fixture's `user_input` against a real provider executor.
    /// Skips `provider_script` entirely — the LLM does the work. Used by
    /// the server's `/v1/eval/online` endpoint and by dataset runs with
    /// a `models` axis override.
    async fn replay_live(
        &self,
        fixture: &Fixture,
        executor: Arc<dyn LlmExecutor>,
        upstream_model: String,
        agent_overrides: Option<AgentSpecPatch>,
        max_total_tokens: Option<u32>,
        revise: Option<ReviseConfig>,
    ) -> ReplayOutcome {
        let sink = InMemorySink::new();
        let plugin = match &self.tee_sink {
            Some(tee) => {
                let composite = CompositeSink::builder()
                    .with_sink(Arc::new(sink.clone()))
                    .with_sink(tee.clone())
                    .build();
                ObservabilityPlugin::new(composite).with_provider(LIVE_PROVIDER_ID)
            }
            None => ObservabilityPlugin::new(sink.clone()).with_provider(LIVE_PROVIDER_ID),
        };

        let store = Arc::new(InMemoryStore::new());
        // Live mode has no script to bound max_rounds against; use the
        // floor as-is. Operators wanting more rounds set the floor.
        let max_rounds = self.max_rounds_floor;

        // Base agent spec: either the caller-supplied registered spec
        // (so the eval exercises the real agent's system_prompt / tool
        // list) or a synthetic stub.
        let base = self.agent_base.as_deref().cloned().unwrap_or(AgentSpec {
            id: DEFAULT_AGENT_ID.into(),
            model_id: LIVE_MODEL_ID.into(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_rounds,
            plugin_ids: vec!["observability".into()],
            ..Default::default()
        });
        let mut agent_spec = match agent_overrides {
            Some(patch) => merge_agent_spec(base, patch),
            None => base,
        };
        // Force three fields back to safe defaults — both the registered
        // base and the override may carry production values that don't
        // belong in an eval:
        //   * `model_id` — the matrix cell pins this; LIVE_MODEL_ID is
        //     what the synthetic registry binds to the test executor.
        //   * `id` — DEFAULT_AGENT_ID is what AgentRuntimeBuilder
        //     registers below; mismatches drop the run.
        //   * `plugin_ids` — observability only, so the eval cannot
        //     trigger side-effectful plugins (mcp_*, skills with HTTP,
        //     …) by inheriting them from the registered spec.
        agent_spec.model_id = LIVE_MODEL_ID.into();
        agent_spec.id = DEFAULT_AGENT_ID.into();
        agent_spec.plugin_ids = vec!["observability".into()];
        agent_spec.max_rounds = max_rounds;

        let runtime: Arc<AgentRuntime> = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider(LIVE_PROVIDER_ID, executor)
                .with_model_binding(
                    LIVE_MODEL_ID,
                    ModelBinding {
                        provider_id: LIVE_PROVIDER_ID.into(),
                        upstream_model,
                    },
                )
                .with_thread_run_store(store.clone())
                .with_agent_spec(agent_spec)
                .with_plugin("observability", Arc::new(plugin))
                .build()
                .expect("live runtime builds"),
        );

        let thread_id = format!("eval-thread-{}", fixture.id);
        let start = Instant::now();

        // Iterate through the dialogue: initial user_input + every
        // continued turn, all on the same thread (same-thread reuse —
        // each successive run_to_completion loads the prior turn's
        // history). Multi-turn fixtures (produced by import-dialogue or
        // hand-authored continued_turns) get evaluated as the full
        // conversation; without this loop Live mode would silently
        // truncate to the first turn while scripted mode replayed them
        // all — a confusing divergence the matrix runner would inherit.
        let dialogue_inputs: Vec<&str> = std::iter::once(fixture.user_input.as_str())
            .chain(
                fixture
                    .continued_turns
                    .iter()
                    .map(|t| t.user_input.as_str()),
            )
            .collect();
        let mut final_text = String::new();
        let mut last_error: Option<String> = None;
        let mut dialogue_ok = true;
        for input in dialogue_inputs {
            let request = RunRequest::new(thread_id.clone(), vec![Message::user(input)])
                .with_agent_id(DEFAULT_AGENT_ID);
            match runtime.run_to_completion(request).await {
                Ok(r) => {
                    final_text = r.response;
                    last_error = None;
                }
                Err(err) => {
                    // First-error short-circuit: continuing past a turn
                    // that already errored would just stack more failures
                    // against an undefined thread state.
                    final_text = String::new();
                    last_error = Some(err.to_string());
                    dialogue_ok = false;
                    break;
                }
            }
        }

        let mut revision_count: u32 = 0;
        let mut judge_score: Option<f32> = None;
        let mut judge_reasoning: Option<String> = None;

        // Reprocess-on-judge-fail loop. Only fires when the full
        // dialogue (initial turn + all continued_turns) completed
        // successfully — an Err on any turn short-circuits, and judging
        // an empty failed response would feed noise back into the agent.
        if let Some(cfg) = revise.as_ref()
            && dialogue_ok
        {
            // Feed the judge a minimal stub outcome — judges only read
            // final_text + user_prompt + rubric. Allocating full metrics
            // (spans, etc.) per retry would be wasted work.
            for _ in 0..=cfg.max_retries {
                let stub = judge_stub_outcome(&fixture.id, &final_text);
                match cfg
                    .judge
                    .judge(&stub, &fixture.user_input, cfg.rubric.as_deref())
                    .await
                {
                    Ok(jr) => {
                        judge_score = Some(jr.score);
                        judge_reasoning = jr.reasoning.clone();
                        if jr.score >= cfg.threshold {
                            break;
                        }
                        if revision_count >= cfg.max_retries {
                            break;
                        }
                        // Compose revision prompt and re-run on same thread.
                        let reasoning = jr.reasoning.as_deref().unwrap_or("(no reasoning given)");
                        let revise_msg = format!(
                            "Your prior answer was:\n\"{final_text}\"\n\nThe grader scored it \
                             {score:.2} (threshold {threshold:.2}). Reasoning: {reasoning}\n\n\
                             Please revise your answer to address the feedback.",
                            score = jr.score,
                            threshold = cfg.threshold,
                        );
                        let request =
                            RunRequest::new(thread_id.clone(), vec![Message::user(revise_msg)])
                                .with_agent_id(DEFAULT_AGENT_ID);
                        revision_count += 1;
                        match runtime.run_to_completion(request).await {
                            Ok(r) => {
                                final_text = r.response;
                                last_error = None;
                            }
                            Err(err) => {
                                last_error = Some(err.to_string());
                                final_text = String::new();
                                break;
                            }
                        }
                    }
                    Err(_) => break, // judge transport failure — keep last good outcome
                }
            }
        }

        let elapsed = start.elapsed();
        let error_type = last_error.clone();

        // Post-hoc token budget — see ReplayMode::Live::max_total_tokens
        // docstring for why this isn't real-time cancellation.
        let metrics = sink.metrics();
        let total_tokens = metrics_total_tokens(&metrics);
        let runtime_failure = match (max_total_tokens, last_error.as_deref()) {
            (Some(max), _) if total_tokens > max => Some(ReplayRuntimeFailure::RuntimeError {
                message: format!("token budget exceeded: {total_tokens} > max {max}"),
            }),
            (_, Some(msg)) => Some(ReplayRuntimeFailure::RuntimeError {
                message: msg.to_string(),
            }),
            _ => None,
        };

        ReplayOutcome {
            fixture_id: fixture.id.clone(),
            final_text,
            metrics,
            elapsed,
            error_type,
            inference_error_count: 0,
            runtime_failure,
            revision_count,
            judge_score,
            judge_reasoning,
        }
    }
}

/// Stub outcome handed to [`crate::judge::Judge::judge`] inside the
/// revise loop. Only the fields the judge actually reads (`fixture_id`,
/// `final_text`) are meaningful; everything else stays at default. Kept
/// as a free fn so callers don't accidentally pass it where a *real*
/// outcome is expected.
fn judge_stub_outcome(fixture_id: &str, final_text: &str) -> ReplayOutcome {
    ReplayOutcome {
        fixture_id: fixture_id.to_string(),
        final_text: final_text.to_string(),
        metrics: awaken_ext_observability::AgentMetrics::default(),
        elapsed: std::time::Duration::ZERO,
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    }
}

/// Sum of `total_tokens` across all inference spans, clamping negative
/// values to zero. Mirrors `ReplayOutcome::total_tokens()` but works on
/// `AgentMetrics` directly (no built outcome yet at the cap-check site).
fn metrics_total_tokens(metrics: &awaken_ext_observability::AgentMetrics) -> u32 {
    let total: i64 = metrics
        .inferences
        .iter()
        .map(|s| {
            if let Some(t) = s.total_tokens {
                i64::from(t).max(0)
            } else {
                let input = i64::from(s.input_tokens.unwrap_or(0)).max(0);
                let output = i64::from(s.output_tokens.unwrap_or(0)).max(0);
                input + output
            }
        })
        .sum();
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// Pick the single most diagnostic [`ReplayRuntimeFailure`] for a
/// completed replay. Precedence (highest first):
///
///  1. **`ScriptExhausted`** — the executor was called when its script
///     was empty. Proves the runtime asked for more events than the
///     fixture promised; outranks everything because it points directly
///     at the runtime contract violation.
///  2. **`RuntimeError`** — the run returned `Err` and no scripted event
///     captured it. Catches non-scripted failures (model-guard mismatch,
///     resolver error, internal bug). Must outrank
///     `ProviderScriptUnused`: a `RuntimeError` often leaves the script
///     untouched (e.g. upstream_model guard rejects before popping), so
///     reporting "script unused" would hide the real cause.
///  3. **`ProviderScriptUnused`** — the run completed or failed via a
///     *scripted* error without consuming the whole script. Genuine
///     "runtime stopped early" territory.
fn decide_runtime_failure(
    exhausted_calls: usize,
    remaining: usize,
    runtime_error_message: Option<String>,
    has_scripted_error: bool,
    allow_unused: bool,
) -> Option<ReplayRuntimeFailure> {
    if exhausted_calls > 0 {
        return Some(ReplayRuntimeFailure::ScriptExhausted {
            extra_calls: exhausted_calls,
        });
    }
    // Scripted error path: runtime returned Err but a scripted Error
    // event captured it — the run failed *as expected*, only unused
    // script remains a fixture-contract concern (handled below).
    if let Some(message) = runtime_error_message
        && !has_scripted_error
    {
        return Some(ReplayRuntimeFailure::RuntimeError { message });
    }
    if remaining > 0 && !allow_unused {
        return Some(ReplayRuntimeFailure::ProviderScriptUnused { remaining });
    }
    None
}

#[cfg(test)]
#[path = "runtime_replayer_test.rs"]
mod tests;
