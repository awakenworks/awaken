# Managed Agents behavior conformance

This repository serves a Claude Managed Agents (CMA) shaped surface: the official
Anthropic SDKs drive it, and `e2e/conformance/` already proves a great deal about
that surface — request and response contracts for the TypeScript and Python SDKs,
an event catalog with golden snapshots, a per-event behavior manifest binding
each event family to a named test, webhook and environment-worker contracts, and
a candidate-delta gate that runs against new SDK releases before they land.

What that suite establishes is that the **shapes** agree. It does not establish
that the **promises behind the shapes** agree, and those are what a client acts
on. CMA documents validation rules whose rejection a client is entitled to rely
on, and composition rules that bound what a delegated agent can reach. A body
this surface accepts but CMA would refuse is a body no CMA deployment could have
produced; a limit CMA guarantees and this surface does not is a guarantee a
client may have built a safety argument on.

A golden snapshot cannot catch either class, because in both the wire shape is
valid. That is the gap this document inventories.

## Verified status, 2026-09-29

Checked against `agent_config_from_managed` on the managed agent config surface.
"Accepted" means the violating body parsed without error.

| CMA rule | CMA states | This surface | Covered by |
| --- | --- | --- | --- |
| Roster holds 1–20 entries | `agents` lists 1-20 roster entries | accepted 21, accepted 0 | baseline entry |
| At most one `self` | at most one `self` per roster | accepted two | baseline entry |
| Roster names unique | names must be unique across the roster | accepted duplicates | baseline entry |
| No member named `self` | don't name an agent `self` | accepted | baseline entry |
| At most one advisor | at most one advisor entry per roster | accepted two | baseline entry |
| `anthropic.advisor` reserved | a member literally named it is a 400 | accepted | baseline entry |
| Delegation is one level | one level only, "enforced rather than silently flattened" | `DelegationLimits::default()` is `(8, 8, 64)` — depth **8** | baseline entry |

Those seven are pinned by `crates/control/awaken-config-service/tests/cma_conformance.rs`
against `contracts/cma-conformance-baseline.json`. The baseline is a ratchet, not
a snapshot: a newly accepted violation fails the test, a rule that starts
rejecting fails until its entry is deleted, and an entry naming no checked rule
fails as a rename or typo. Pinning today's acceptance as "expected" would convert
a divergence into a contract, which is the opposite of conformance.

## Not covered, and where it belongs

These are runtime and session semantics, so the config-plane test cannot reach
them. They are listed here rather than added to that baseline, because a baseline
implies something checks the entry.

- **`auto` sees no session content.** CMA evaluates each call over "tool + input
  + session content so far" and may deny it as high-risk.
  `AutoPermissionEvaluator::evaluate(&self, call: &ToolCall)` is synchronous and
  receives only the normalized call, so session content is not merely unused —
  it is unreachable through the port. `AutoDecision` does carry `Deny`, but the
  default `ObservingToolEvaluator` never returns it, and says why: a name-based
  classifier cannot support the claim that a call is too risky for anyone to
  approve. The divergence is fail-safe (more calls fall to a person, none are
  silently auto-run that CMA would have denied) but it is a divergence, and
  whether a deployment conforms depends on the evaluator it injects.
- **The inter-thread trust boundary is vacuous here.** CMA states that only a
  client's `user.message` on the primary thread carries operator intent, and that
  a message between session threads carries none. With no session content in the
  evaluator there is nothing for that rule to range over, so this surface does
  not violate it — it cannot express it. The rule becomes load-bearing the moment
  an evaluator that reads session content is injected, and deeper delegation
  raises the stakes: more hops mean more chances for authority to accumulate.
- **Web domain lists do not compose.** CMA's one genuinely composing rule is
  `allowed_domains` / `blocked_domains` along the path to a thread — allow-lists
  intersect, block-lists union, "a roster agent can narrow but never widen" —
  while `max_content_tokens` and `user_location` deliberately do *not* combine
  (own, then caller's, then coordinator's). This surface exposes all five as
  configurable fields on `web_search` / `web_fetch` and has no composition step.
  A test belongs beside the delegation runtime, and must cover the non-combining
  pair too, or it will encode "intersect everything", which CMA explicitly is not.
- **`agent_with_overrides` propagation.** CMA applies session overrides to the
  coordinator and its `self` copies, and never to a roster agent referenced by ID.
  The override surface exists here; the propagation rule is untested.
- **`user.tool_confirmation` for a non-`ask` event.** CMA rejects it with a 400,
  including for a call it denied under `auto`, so a client cannot override a
  denial. Not verified either way in this pass — the inbound validation path was
  not traced, and the projection site found instead only re-emits a confirmation.

## The problems, ranked

1. **A guarantee is contradicted by default, not by opt-in.** A coordinator body
   that states no limits resolves to eight levels of delegation. A CMA client
   reads the same `multiagent` roster and is entitled to reason that a roster
   member cannot delegate further — a premise that bounds how far authority can
   travel. The premise does not hold here, and nothing in the body says so.
2. **The projection carries undeclared extension fields.** `delegation_limits`
   is emitted onto the managed agent shape, which has no such field in CMA, and
   it is not the only one: the projection also emits `max_steps`,
   `context_policy`, `recovery_policies`, `tool_discovery`, `tool_exposure`,
   `tool_patterns`, `plugin_config`, `plugins`, `model_candidates`, `inference`
   and `compaction`. Extensions are legitimate — this repository deliberately
   exceeds CMA — but an extension that carries a limit contradicting a CMA
   guarantee needs to say so where a client will see it.
3. **The wire-naming guard has a blind spot exactly here.**
   `check_managed_wire_naming.py` ratchets annotations for
   `awaken-protocol-managed` types against that crate's baseline. The managed
   agent config projection lives in `awaken-config-service`, so none of the
   fields in problem 2 are subject to it, and none are documented.
4. **Every roster validation CMA documents is absent.** Seven rules, all
   accepted. Individually each is minor; together they mean a roster that no CMA
   deployment would store can be stored here, so a config that round-trips
   successfully is not evidence it would work against CMA.
5. **Conformance coverage is shaped like the suite's strength.** Event shapes and
   SDK surfaces are covered well; decision semantics are not covered at all.
   Searching `e2e/conformance/` finds no reference to `agent_with_overrides`, to
   `allowed_domains` / `blocked_domains`, or to `budget_reached`.

## What consistency should mean here

Feature parity is not the goal — this repository is positioned not to lock a
model, cloud, or protocol, and deeper delegation is a deliberate excess. Two
narrower things are the goal, and both are testable:

- **Contract consistency.** What this surface projects as a CMA agent must be a
  body CMA would accept, and a limit that contradicts a CMA guarantee must not
  ride silently inside it. The failure mode to design against is not a missing
  feature; it is a client making a safety decision on a premise that does not
  hold.
- **Semantic consistency within the conformant subset.** Where a rule is
  expressible here, the same inputs should reach the same decision. Where it is
  not expressible — `auto` without session content — the honest move is to record
  the boundary, not to approximate the rule.

The ratchet is the mechanism for both: a rule is either enforced, or it is named
in a list that can only shrink.
