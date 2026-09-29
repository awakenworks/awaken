# Managed Agents behavior conformance

This repository serves a Claude Managed Agents (CMA) shaped surface, and
`e2e/conformance/` already proves a great deal about it: request and response
contracts for the official TypeScript and Python SDKs, an event catalog with
golden snapshots, a per-event behavior manifest binding each event family to a
named test, webhook and environment-worker contracts, and a candidate-delta gate
that runs against new SDK releases before they land.

Those establish that the **shapes** agree. This document is about the **promises
behind the shapes**, which a golden snapshot cannot reach: in a promise failure
the wire shape is still valid.

## The roster rules are enforced, and now pinned

Every documented CMA roster rule is decided by an owner. Two owners, split by
what each can see:

**`MultiagentConfig::validate`** (`awaken-agent-config/src/config.rs`) decides
the rules that are properties of the roster alone. Managed admission, publication
compilation and the managed projection all call it, so a roster that reaches a
session has passed it. Pinned by
`awaken-config-service/tests/cma_conformance.rs`:

| CMA rule | CMA states |
| --- | --- |
| At least one entry | `agents` lists 1-20 roster entries |
| At most twenty entries | `agents` lists 1-20 roster entries |
| At most one `self` | at most one `self` entry per roster |
| Names unique | names must be unique across the roster |
| Recursion uses `self` | the coordinator is reached through the `self` entry |
| At most one advisor | at most one advisor entry per roster |
| Advisor states a model | the entry has exactly two fields, `type` and `model` |
| `anthropic.advisor` reserved | a member literally named it is a 400 |
| Pinned version at least 1 | omit `version` to pin the latest at save time |

**`resolve_multiagent_references`** (`awaken-control/src/managed_agents.rs`)
decides the rules that need the referenced revision loaded. Pinned by the R1-R5
table beside it, with the geo rule keeping its own G1-G3 table:

| CMA rule | How it is refused |
| --- | --- |
| **One level of delegation** | a member that is itself a coordinator is refused, not flattened |
| Roster-uniform inference geo | the frozen geo of each reference must equal the coordinator's |
| Reference must exist | an unpublished id is refused |
| Reference must be live | a disabled or archived reference is refused |
| Pinned version must exist | a version never written is refused |

Both tables assert a positive witness as well, so neither can pass by refusing
everything, and each refusal is checked to leave no coordinator behind.

There is no recorded gap. An earlier revision of this document carried a baseline
of rules believed unenforced; it was deleted because every entry in it turned out
to be decided.

## Three readings that were wrong

Recorded because each looked like a finding, and the sequence is the useful part:

1. **Probing `agent_config_from_managed`.** That function *parses* a managed body;
   admission is elsewhere. Authoring under `/v1/config/agents` is deliberately
   permissive - `put` does not validate, `/validate` is the author's dry run, and
   publication compilation is the gate. Measured at the parser, all nine shape
   rules look unenforced. They are not.
2. **Reading `delegation_limits` as a CMA field.** It is emitted by
   `/v1/config/agents`, Awaken's own config plane, whose vocabulary is
   intentionally richer than CMA's. A CMA client never sees that surface. The CMA
   surface is `awaken-protocol-managed/src/routes/agents_registry.rs` with typed
   `AgentCreateParams`, which the wire-naming guard does cover.
3. **Reading `DelegationLimits::default()` as a CMA depth divergence.** Its
   `max_depth` of 8 is consumed only by `admit_new_delegation`, the Runtime's
   parent-Run/child-Run kernel. The CMA roster is a different mechanism, and a
   roster member is refused if it carries a roster of its own, so it has no
   delegation target and cannot spawn a child. Depth beyond one level is
   unreachable through the CMA path; the budget governs a feature a CMA-shaped
   agent does not enter.

Each was found by reading one layer further down. The lesson worth keeping is
that a rule measured at the wrong layer reports a gap that is not there, and the
measurement looks identical either way.

## Runtime semantics still uncovered

Session-scoped, so neither owner above reaches them. Listed here rather than in a
baseline, because a baseline entry implies something checks it.

- **`auto` sees no session content.** CMA evaluates each call over "tool + input
  + session content so far" and may deny it as high-risk.
  `AutoPermissionEvaluator::evaluate(&self, call: &ToolCall)` is synchronous and
  receives only the normalized call. `AutoDecision` carries `Deny`, but the
  default `ObservingToolEvaluator` never returns it and says why: a name-based
  classifier cannot support the claim that a call is too risky for anyone to
  approve. The divergence is fail-safe - more calls fall to a person - but
  whether a deployment conforms depends on the evaluator it injects.
- **The inter-thread trust boundary is vacuous here.** CMA states that only a
  client's `user.message` on the primary thread carries operator intent, and a
  message between session threads carries none. With no session content in the
  evaluator there is nothing for the rule to range over, so this surface does not
  violate it - it cannot express it. The rule becomes load-bearing the moment an
  evaluator that reads session content is injected.
- **Web domain lists do not compose.** CMA's one genuinely composing rule is
  `allowed_domains` / `blocked_domains` along the path to a thread - allow-lists
  intersect, block-lists union, "a roster agent can narrow but never widen" -
  while `max_content_tokens` and `user_location` deliberately do *not* combine
  (own, then caller's, then coordinator's). This repository exposes all five as
  configurable fields and has no composition step. A test belongs beside the
  delegation runtime and must cover the non-combining pair, or it will encode
  "intersect everything", which CMA explicitly is not.
- **`agent_with_overrides` propagation.** CMA applies session overrides to the
  coordinator and its `self` copies, never to a roster agent referenced by id.
  The override surface exists here; the propagation rule is untested.
- **`user.tool_confirmation` for a non-`ask` event.** CMA rejects it with a 400,
  including for a call denied under `auto`, so a client cannot override a denial.
  Three attempts to trace the inbound validation found only the outbound
  projection (`RecordedPermission::from_audit`, which is exact and fail-closed).
  Not verified either way; not asserted either way.

## Why a concurrency scanner is not the answer here

One e2e row - `managed_memory_repository_durable_e2e.mjs`, `U1/U2
true/simultaneous` - failed under gate load and passed standalone, which invites
a guard that flags racing tests. The narrow signal (an `allSettled` result
filtered by `fulfilled` and asserted by count or index) was measured against
every candidate in `e2e/`: four match, and two are sound - one races two clients
writing *different* content, where exactly one winner is correct in every
interleaving; the other partitions results only for cleanup. Fifty per cent
precision produces a guard people learn to ignore.

The defect was never the race primitive. It was an expectation that changed with
the interleaving: identical content yielded two successes only when the loser
read after the winner committed. That is a property of the expectation, not a
pattern in the syntax, and the fix was to make the production behavior
interleaving-independent - after which the row is deterministic and needs no
guard. Removing the nondeterminism beat detecting it.

## Where the guard against scope drift already lives

`scripts/ci/check_scan_scope.py` ratchets every hand-written scope inside
`scripts/ci`, with `scan_scope_registry.json` holding the reason each one is not
derived from its subject. Its message states the principle better than a
paraphrase would: *a guard whose scope is written by hand is a guard that will
one day report OK over code it no longer reads.* Declaring `INPUT_GLOBS` on a
checker lowers that baseline; the gate refuses a baseline that has not been
lowered after ground was gained, and refuses an entry naming a checker that now
declares its inputs.

A second mechanism for the same purpose was written during this work and deleted:
`generate_hooks.py` already renders every trigger from the checker that owns the
inputs, and the registry already ratchets the exceptions. Two guards for one
invariant is the duplication this repository eliminates first.
