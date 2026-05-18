//! Runtime-backed [`Replayer`] that builds a real [`AgentRuntime`] with a
//! per-fixture [`ScriptedLlmExecutor`] and harvests the resulting
//! observability spans into a [`ReplayOutcome`].
//!
//! Unlike [`crate::replay::MockReplayer`], which synthesised a single
//! `GenAISpan` from heuristics, `RuntimeReplayer` exercises the full agent
//! loop and lets `awaken-ext-observability` record real spans. For
//! fixtures driven by an explicit `provider_script`, every token count
//! and stop reason in the resulting `AgentMetrics` comes straight from
//! the script. Legacy `mock_response: { kind: "text" }` fixtures still
//! load through [`Fixture::effective_script`], which seeds `TokenUsage`
//! with a `chars / 4` estimate to preserve the original
//! `max_tokens_total` semantics until those fixtures migrate.
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
pub enum ReplayMode {
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
        /// onto the default replay agent.
        agent_overrides: Option<AgentSpecPatch>,
        /// Post-hoc token budget. After replay completes, if
        /// `outcome.total_tokens() > max`, a
        /// [`ReplayRuntimeFailure::RuntimeError`] is recorded with a
        /// `"token budget exceeded"` message. Real-time cancellation
        /// requires a cancellation token plumbed through the runtime —
        /// that's a follow-up; the soft cap catches "this fixture cost
        /// $X" without aborting expensive in-flight inference.
        max_total_tokens: Option<u32>,
    },
}

impl Default for ReplayMode {
    fn default() -> Self {
        Self::Scripted
    }
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
    /// Replay mode — Scripted (default) or Live.
    mode: ReplayMode,
}

impl RuntimeReplayer {
    pub fn new() -> Self {
        Self {
            max_rounds_floor: 4,
            tee_sink: None,
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
        };
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
            *agent_overrides = Some(patch);
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
            } => {
                self.replay_live(
                    fixture,
                    executor.clone(),
                    upstream_model.clone(),
                    agent_overrides.clone(),
                    *max_total_tokens,
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

        // Build the synthetic agent: default base merged with caller
        // overrides (model_id force-pinned to LIVE_MODEL_ID).
        let base = AgentSpec {
            id: DEFAULT_AGENT_ID.into(),
            model_id: LIVE_MODEL_ID.into(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_rounds,
            plugin_ids: vec!["observability".into()],
            ..Default::default()
        };
        let mut agent_spec = match agent_overrides {
            Some(patch) => merge_agent_spec(base, patch),
            None => base,
        };
        // Force model_id back to LIVE_MODEL_ID — the override may have
        // set its own value but the live model is what's under test,
        // by definition.
        agent_spec.model_id = LIVE_MODEL_ID.into();
        // Live mode preserves the agent_id we use for routing; if the
        // override changed it, fix that too so the RunRequest lands.
        agent_spec.id = DEFAULT_AGENT_ID.into();

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

        let request = RunRequest::new(
            format!("eval-thread-{}", fixture.id),
            vec![Message::user(&fixture.user_input)],
        )
        .with_agent_id(DEFAULT_AGENT_ID);

        let start = Instant::now();
        let outcome = runtime.run_to_completion(request).await;
        let elapsed = start.elapsed();

        let final_text = match &outcome {
            Ok(result) => result.response.clone(),
            Err(_) => String::new(),
        };
        let error_type = match &outcome {
            // Live mode: the runtime owns the error variant. Surface its
            // toString so the scorer's `expected_error_type` assertion
            // still works for live-mode failure fixtures.
            Err(err) => Some(err.to_string()),
            Ok(_) => None,
        };

        // Post-hoc token budget — see ReplayMode::Live::max_total_tokens
        // docstring for why this isn't real-time cancellation.
        let metrics = sink.metrics();
        let total_tokens = metrics_total_tokens(&metrics);
        let runtime_failure = match (max_total_tokens, outcome.as_ref()) {
            (Some(max), _) if total_tokens > max => Some(ReplayRuntimeFailure::RuntimeError {
                message: format!("token budget exceeded: {total_tokens} > max {max}"),
            }),
            (_, Err(err)) => Some(ReplayRuntimeFailure::RuntimeError {
                message: err.to_string(),
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
        }
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
