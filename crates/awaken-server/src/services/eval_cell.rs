//! Per-cell composition primitives shared by the dataset matrix runner
//! (`eval_run_service::run_matrix_cells`) and the ad-hoc online runner
//! (`online_eval_service`). Lifting them out of `eval_run_service` keeps
//! that file under the lefthook line cap and makes the contract between
//! the two drivers explicit instead of cross-module inline duplication.

use std::sync::Arc;

use awaken_contract::agent_spec_patch::AgentSpecPatch;
use awaken_eval::{
    Expectation, Fixture, LlmExecutorJudge, RuntimeReplayer, score, score_with_judge,
};

use crate::error::ApiError;

/// Resolved judge config carried through the replay loop. Replaces the
/// `(LlmExecutorJudge, Option<String>, Option<u32>)` tuple this code used
/// to thread — at the call sites that tuple was opaque enough that
/// renaming or reordering a field was a search-and-replace minefield.
pub(crate) struct JudgeContext {
    pub judge: LlmExecutorJudge,
    pub rubric: Option<String>,
    pub revise_max_retries: Option<u32>,
}

/// Per-cell revise loop config: `(judge, rubric, threshold, max_retries)`.
/// `None` when any required piece (judge, fixture threshold, retry budget)
/// is missing — same gating in dataset matrix runs and online ad-hoc runs.
pub(crate) type ReviseTuple = (Arc<dyn awaken_eval::judge::Judge>, Option<String>, f32, u32);

/// Build the per-cell revise tuple, applying the all-three-pieces gating
/// rule both eval services share.
pub(crate) fn revise_tuple_for(
    judge: Option<&JudgeContext>,
    expect: &Expectation,
) -> Option<ReviseTuple> {
    match (judge, expect.min_judge_score) {
        (
            Some(JudgeContext {
                judge: j,
                rubric,
                revise_max_retries: Some(retries),
            }),
            Some(threshold),
        ) => Some((
            Arc::new(j.clone()) as Arc<dyn awaken_eval::judge::Judge>,
            rubric.clone(),
            threshold,
            *retries,
        )),
        _ => None,
    }
}

/// Apply the three optional decorators every cell shares: agent overrides,
/// tee-sink for trace fan-out, and the revise-on-judge-fail loop.
pub(crate) fn apply_cell_decorators(
    mut replayer: RuntimeReplayer,
    overrides: Option<AgentSpecPatch>,
    trace_sink: Option<Arc<dyn awaken_ext_observability::MetricsSink>>,
    revise: Option<ReviseTuple>,
) -> RuntimeReplayer {
    if let Some(p) = overrides {
        replayer = replayer.with_agent_overrides(p);
    }
    if let Some(sink) = trace_sink {
        replayer = replayer.with_tee_sink(sink);
    }
    if let Some((j, rubric, threshold, retries)) = revise {
        replayer = replayer.with_revise_on_judge_fail(j, rubric, threshold, retries);
    }
    replayer
}

/// Pick the scorer based on whether a judge is wired: judge-aware when a
/// `JudgeContext` is present AND the fixture asks for it via
/// `min_judge_score`; otherwise the deterministic scorer.
pub(crate) async fn score_outcome(
    outcome: &awaken_eval::ReplayOutcome,
    fixture: &Fixture,
    judge: Option<&JudgeContext>,
) -> Result<Vec<awaken_eval::Failure>, ApiError> {
    match (judge, fixture.expect.min_judge_score) {
        (
            Some(JudgeContext {
                judge: j, rubric, ..
            }),
            Some(_),
        ) => {
            let (failures, _) = score_with_judge(
                outcome,
                &fixture.expect,
                &fixture.user_input,
                rubric.as_deref(),
                j,
            )
            .await
            .map_err(|err| ApiError::Internal(format!("judge invocation failed: {err}")))?;
            Ok(failures)
        }
        _ => Ok(score(outcome, &fixture.expect)),
    }
}
