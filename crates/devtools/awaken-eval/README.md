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
