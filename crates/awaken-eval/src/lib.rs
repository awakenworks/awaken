//! Fixture-driven replay and scoring framework for awaken agent runs.
//!
//! `awaken-eval` lets you snapshot an agent's expected behaviour against a
//! reproducible scenario file (a [`Fixture`]) and score subsequent runs
//! against that expectation, producing an NDJSON report that CI can diff
//! against a committed baseline.
//!
//! The framework is split into three pure layers:
//!
//! 1. **Types** ([`Fixture`], [`Expectation`], [`Failure`], [`ReplayOutcome`],
//!    [`ReplayReport`]) — serializable, deterministic, no I/O.
//! 2. **Scoring** ([`score`]) — pure function from
//!    `(ReplayOutcome, Expectation)` to `Vec<Failure>`.
//! 3. **Replay** (M4.3) and **Reporting** (M4.2) — orchestrate the pure
//!    layers against a real or mock agent runtime.
//!
//! All layers integrate with the 0.4 [`awaken-ext-observability`] surface
//! and never modify it; `AgentMetrics` is what the scorer consumes.

pub mod curate;
pub mod dataset;
pub mod eval_run;
pub mod expectation;
pub mod fixture;
pub mod judge;
pub mod outcome;
pub mod replay;
pub mod report;
pub mod runtime_replayer;
pub mod score;

pub use curate::{CurateError, TraceConversion, trace_to_provider_script};
pub use dataset::{DATASETS_NAMESPACE, DatasetSpec};
pub use eval_run::{
    EvalRun, EvalRunFilter, EvalRunItem, EvalRunStore, EvalRunStoreError, EvalRunSummary,
    FileEvalRunStore, MatrixCell, SampleAggregate, expand_cells, mint_run_id,
};
pub use expectation::{Expectation, Failure};
pub use fixture::load_directory;
pub use fixture::{Fixture, FixtureError, MockResponse};
pub use outcome::{ReplayOutcome, ReplayReport};
pub use replay::{MockReplayer, Replayer, replay_all};
pub use report::{
    DiffEntry, DiffSummary, ReportError, diff_against_baseline, diff_eval_items, read_ndjson,
    read_ndjson_path, write_ndjson, write_ndjson_path,
};
pub use runtime_replayer::RuntimeReplayer;
pub use score::score;

pub use judge::{
    Judge, JudgeConfig, JudgeError, JudgeResult, LlmExecutorJudge, TensorZeroJudge,
    score_with_judge,
};
