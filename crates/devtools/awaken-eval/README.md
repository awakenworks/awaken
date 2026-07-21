# Auxiliary Agent evaluation

`awaken-eval` evaluates the production Outcome Judge, Compact, and Memory
contracts without adding a second orchestration path. Live providers run through
the same ACP `RunExecutor`; production prompts and strict parsers are reused.

The three contracts remain separate:

- Judge: three-state decision accuracy, schema compliance, unsafe accepts, and
  evidence grounding;
- Compact: current-fact preservation and stale/superseded fact deletion;
- Memory extraction: durable-fact recall and forbidden-fact deletion; Memory
  selection: strict protocol, exact set, precision, and recall.

They share only execution and artifact plumbing. A good Judge score cannot hide
context loss in Compact or unsafe persistence in Memory.

## Evaluation boundaries

The Agent Judge owns only three business decisions:

- `satisfied`: the rubric is fully met;
- `needs_revision`: another Worker iteration could improve an incomplete result;
- `failed`: an explicit unrecoverable business failure or policy prohibition.

Infrastructure errors, interruption, iteration exhaustion and crash redrive are
Runtime lifecycle decisions. They remain deterministic state-machine tests and
must not be added as Judge labels.

The harness reports both exact three-class accuracy and binary
satisfied-vs-not-satisfied agreement. The binary metric is required for Claude
Code `/goal` records because `goal_status.met` has no failed/revision distinction.

## Private transcript corpora

Importers read local transcripts but write a lossy derived dataset:

- Claude Code: `attachment.type == "goal_status"`; the completion condition and
  preceding assistant deliverable become the case, while `met` is the weak label.
  The evaluator's `reason` is deliberately not copied.
- Codex: `thread_goals.status == "complete"` plus the corresponding rollout's
  `update_goal({status:"complete"})`; the post-completion final answer is the
  deliverable. These labels are positive-only and self-judged.

Both importers remove the local home prefix, omit session/provider identifiers,
deduplicate records, and tag every case `weak_oracle`. Generated private corpora
and provider outputs belong outside the repository; do not commit them.

```bash
cargo run -p awaken-eval -- outcome-import-claude \
  ~/.claude/projects /tmp/claude-goals.json 120

cargo run -p awaken-eval -- outcome-import-codex \
  ~/.codex/sessions ~/.codex/goals_1.sqlite /tmp/codex-goals.json
```

Weak-oracle agreement is diagnostic, not a release gate. Disagreements require
independent adjudication against repository state, diffs and test evidence.

## Live ACP screening

The subprocess environment is cleared by the ACP launcher. Pass only explicit
adapter configuration; normal HOME-backed provider authentication remains
available. Judge runs use read-only mode and a deny-all tool permission policy.
The ACP protocol has no portable system-prompt field, so the unified ACP
`RunExecutor` projects frozen snapshot instructions before the untrusted first
input. Later steers reuse the ACP session and do not repeat that stable prefix,
which preserves provider prefix-cache locality.

```bash
export AWAKEN_EVAL_ACP_ENV_JSON='{
  "CODEX_CONFIG":"{\"model\":\"gpt-5.3-codex-spark\",\"model_reasoning_effort\":\"low\",\"approval_policy\":\"never\",\"sandbox_mode\":\"read-only\"}"
}'

cargo run -p awaken-eval -- outcome-run-acp \
  /tmp/claude-goals.json /tmp/claude-goals-acp.json \
  '["npx","-y","@agentclientprotocol/codex-acp@1.1"]' 20

cargo run -p awaken-eval -- outcome-select-acp-failures \
  /tmp/claude-goals.json /tmp/claude-goals-acp.json \
  /tmp/claude-goals-retry.json
```

Compact and Memory gold runs use one independent ACP run per case:

```bash
cargo run -p awaken-eval -- compact-run-acp \
  crates/devtools/awaken-eval/fixtures/compact-gold-v1.json \
  /tmp/compact-acp.json \
  '["npx","-y","@agentclientprotocol/codex-acp@1.1"]'

cargo run -p awaken-eval -- memory-run-acp \
  crates/devtools/awaken-eval/fixtures/memory-gold-v1.json \
  /tmp/memory-acp.json \
  '["npx","-y","@agentclientprotocol/codex-acp@1.1"]'
```

Memory extraction uses a tool-free, strict JSON projection of proposed
`write_memory` calls to isolate decision quality. The production tool protocol,
filter, persistence, and recall lifecycle are tested in `awaken-ext-memory` and
`awaken-runtime-host`; the projection does not replace those tests.

On 2026-07-22, the stress-floor configuration
`gpt-5.3-codex-spark` / `low` produced 5/5 exact extraction and 6/6 exact
selection. Compact preserved 19/19 required facts and dropped 4/6 stale facts,
with 6/8 cases exact; the two residual stale-history failures remain visible as
model-floor limitations rather than being hidden by a permissive scorer.

Large batches are a cost-oriented screen, not the production request shape. Keep
their first-pass schema compliance as a separate reliability metric. Retry a
rejected batch at size five or one; never overwrite the first-pass result.

## Test design and gates

The corpora use equivalence partitions, boundary cases
(partial completion and exact thresholds), decision tables (rubric × evidence ×
worker state), adversarial cases (prompt injection and conflicting evidence),
and metamorphic pairs (same evidence with one decisive fact changed).

Release gates should use independently adjudicated gold cases:

- 100% strict schema compliance after one bounded repair;
- zero unsafe accepts on policy and explicit-test-failure cases;
- separately reported exact decision accuracy and satisfaction accuracy;
- grounding scored only where independent evidence tokens were annotated;
- every live failure replayed as one production-shaped Judge turn before triage.

Transcript weak labels, public model-generated labels and batch-screen results
must never be presented as human-ground-truth accuracy.

The former `scripts/runtime-prompt-eval.py` harness was removed: it duplicated
prompts, used the obsolete two-state Judge contract, and reconstructed Agents
through a separate configuration-plane path. All maintained evaluation now
uses versioned fixtures and the unified Run boundary.

## Optimization rounds

This ledger records every live optimization round run on 2026-07-22, including
unchanged and rejected variants. The common setup was
`@agentclientprotocol/codex-acp@1.1`, `gpt-5.3-codex-spark`, `low` reasoning,
read-only sandbox, and deny-all tools. Measurements are single stress-floor
samples unless a bounded retry is shown; they are not estimates of mean quality.

Historic raw outputs remain private `/tmp` artifacts. The frozen fixtures,
scorers, current prompts, and replay commands are committed. The local artifact
map used to re-score the numbers below is:

- J0: `awaken-outcome-gold-v1-acp.json`
- J1: `awaken-outcome-gold-v1-acp-v2.json` and its `-retry` artifact
- J2: `awaken-outcome-gold-v1-acp-single.json`
- J3: `awaken-outcome-gold-v1-acp-after-instructions.json`
- J4: `awaken-outcome-gold-v1-acp-final.json`
- C0-C4: `awaken-compact-gold-v1-acp.json`, then `-v2` through `-v5`
- M0-M2: `awaken-memory-gold-v1-acp.json`, then `-v2` and `-v3`

### Judge

Metrics are `schema / exact / satisfied-vs-not / grounded / unsafe accepts`,
each out of 15 except the explicit five-case retry.

| Round | Change | Result | Effect and decision |
|---|---|---|---|
| J0 | Initial three-label schema without a sufficiently explicit permanent-failure distinction or evidence-locator requirement. | `15 / 10 / 10 / 4 / 5` | All five permanent failures were accepted as `satisfied`; unsafe baseline. |
| J1 | Defined recoverable `needs_revision` versus unrecoverable `failed`; required a decisive evidence token or locator; batches of five. | First pass `10 / 10 / 10 / 9 / 0`; one malformed five-case batch. Retry `5 / 5 / 5 / 4 / 0`; combined `15 / 15 / 15 / 13 / 0`. | Removed five unsafe accepts and improved grounding by 9/15 after bounded retry. Kept; batch wire reliability remains separate from reasoning. |
| J2 | One independent production-shaped Judge turn per case. | `15 / 14 / 14 / 15 / 1` | Grounding reached 15/15; `gold-failed-service-retired` remained unsafe. This replaced repaired batch accuracy as the request-shape floor. |
| J3 | Unified ACP `RunExecutor` projects frozen snapshot instructions before untrusted first input. | `15 / 13 / 14 / 14 / 1` | Proved arbitrary ACP Agents receive the Judge contract. Exact moved -1 while binary safety stayed 14/15, consistent with single-sample variation. Kept for runtime correctness and stable prefixing. |
| J4 | Longer meta-explanation for rubrics phrased as “report/conclude failure”. | `15 / 10 / 12 / 15 / 3` | Exact regressed by 3 and unsafe accepts increased by 2 versus J3. Rejected and reverted. |

The retained Judge prompt is the short, domain-neutral J3 version. A regression
test prevents test, coverage, compiler, commit, or Git policy from leaking into
it; domain requirements belong in each Outcome rubric and evidence.

Transcript-derived corpora are weak-oracle diagnostics, not gold accuracy:

| Corpus/round | Result | Interpretation |
|---|---|---|
| Claude `/goal`, initial | 100/120 schema; 69/120 exact; 85/120 binary; 0 unsafe | `failed` was overused for ordinary incompleteness. |
| Claude `/goal`, after recoverable/permanent distinction | 100/120 schema; 86/120 exact and binary; 0 unsafe | Exact improved by 17/120, binary by 1/120, and failed-overuse disappeared. |
| Claude rejected batch retried as four batches of five | 20/20 schema; 18/20 agreement; 0 unsafe | Smaller bounded retry repaired the wire without overwriting first-pass reliability; combined recoverable binary agreement was 104/120 (86.7%). |
| Codex completed-goal transcripts | 8/8 schema; 5/8 agreement | Three self-completed goals lacked enough final-summary evidence: possible self-judgement false positives or evidence loss. |

### Compact

The fixture has 8 cases, 19 required facts, and 6 stale facts. Metrics are
`exact cases / required preserved / stale dropped`.

| Round | Change | Result | Effect and decision |
|---|---|---|---|
| C0 | Original free-text Compact prompt. | `3/8 / 19/19 / 1/6` | Preservation was strong; the model narrated old decisions and completed work. |
| C1 | Added exact value/deadline retention, untrusted transcript handling, no invention, and current-state-only output. | `3/8 / 19/19 / 1/6` | No change. ACP was not consuming snapshot instructions, so the round diagnosed a runtime seam rather than prompt quality. |
| C2 | Fixed ACP snapshot-instruction projection. | `3/8 / 19/19 / 1/6` | Current-state structure appeared, but stale facts remained in parentheses or completed-status prose. Transport fix kept. |
| C3 | Repeated the minimal DROP protocol in the final `SUMMARIZE_PROMPT`. | `5/8 / 18/19 / 4/6` | Exact +2 and stale deletion +3/6; one durable fact adjacent to injection was over-dropped. |
| C4 | Added two domain-neutral rewrite examples and a mixed-material preservation rule. | `6/8 / 19/19 / 4/6` | Restored required recall to 100% and added one exact case; stale deletion unchanged. Kept. |

The two residual stress-floor failures restate a completed item or old choice as
historical. They remain failures; the scorer was not relaxed to hide pollution.

### Memory

The fixture has 5 extraction and 6 selection cases. Extraction metrics are
`exact / schema / expected found / forbidden dropped`; selection metrics are
`exact / schema / true positives / returned`, with six expected selections.
Historic outputs were re-scored with the final strict parser, so schema expresses
the target contract rather than what the old loose integer extractor admitted.

| Round | Change | Extraction | Selection | Effect and decision |
|---|---|---|---|---|
| M0 | Original taxonomy and loose integer parser. | `2/5 / 5/5 / 4/6 / 3/5` | `1/6 / 1/6 / 1/6 / 1` | Saved one completed implementation note, lost safe facts mixed with secrets/injection, and emitted selector prose. |
| M1 | Added mixed-fact handling, secret/completed-work exclusions, untrusted-data handling, strict `NONE`/`[n]`, and a final `select_input` protocol reminder. | `1/5 / 5/5 / 2/6 / 3/5` | `6/6 / 6/6 / 6/6 / 6` | Selector gained +5 exact and schema-valid cases. Extraction moved -1 because its system instructions still were not delivered over ACP; no false prompt credit. |
| M2 | Applied shared ACP instruction projection with the M1 prompts/protocol. | `5/5 / 5/5 / 6/6 / 5/5` | `6/6 / 6/6 / 6/6 / 6` | Extraction gained +4 exact, recovered all four missing expected facts, and dropped both remaining forbidden terms. Kept. |

Extraction evaluation uses strict JSON proposals to isolate `write_memory`
save/skip reasoning. Production tests separately cover real tool calls, the
implementation-note backstop, persistence, recovery, and recall.

### Retained configuration and cache boundary

- Judge: short domain-neutral three-state policy, strict JSON, no tools, and
  evidence grounding; lifecycle failures stay Runtime-owned.
- Compact: tool-free current-state summary, exact durable values, stale/injected
  deletion, and a final-tail DROP reminder.
- Memory extractor: only `write_memory`, mixed safe/forbidden separation, and
  secret/completed-work exclusion.
- Memory selector: tool-free strict `NONE` or bracketed indices; malformed output
  fails closed.
- All three use independent corpora and metrics over one shared ACP runner.

ACP lacks a portable system-prompt field. The runner sends frozen instructions
once before untrusted first input; later steers reuse the ACP session and send
only new input. This fixes C1/M1 and keeps a stable provider-cache prefix. Cache
token counters were not recorded, so this is a cache-compatible structure claim,
not a measured cache-hit increase.
