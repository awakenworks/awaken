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

pub mod expectation;
pub mod fixture;
pub mod outcome;
pub mod replay;
pub mod report;
pub mod score;

pub use expectation::{Expectation, Failure};
pub use fixture::load_directory;
pub use fixture::{Fixture, FixtureError, MockResponse};
pub use outcome::{ReplayOutcome, ReplayReport};
pub use replay::{MockReplayer, Replayer, replay_all};
pub use report::{
    DiffEntry, DiffSummary, ReportError, diff_against_baseline, read_ndjson, read_ndjson_path,
    write_ndjson, write_ndjson_path,
};
pub use score::score;
