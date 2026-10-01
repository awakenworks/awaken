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

## Historical time provenance

The existing `/v1/sessions` Session, Thread, and Event GET/list responses keep
their SDK-compatible timestamp strings. A historical row without a recorded
time can therefore still carry the legacy `2026-01-01T00:00:00Z` placeholder in
the default response. One exact request header,
`x-awaken-session-time-version: 2`, selects the same history routes with nullable
timestamp fields: a time from the committed Session root or canonical Event
order is rendered, and missing typed evidence becomes JSON `null`. The archived
Session timestamp is nullable when its historical string has no typed source.
An actual persisted `2026-01-01T00:00:00Z` remains a date in v2; response text
never decides provenance.

Absent header selects the unchanged v1 serializer. Repeated or other header
values return 400. SSE remains v1 only and rejects an explicit v2 selector
with 400. A v2 list without time predicates retains unknown-time rows; when a
Session or Event time predicate could include an unknown-time candidate, the
whole request returns 503 instead of silently omitting it. Both versions read
the same committed projection and ownership guard; v2 has no second history
store or projector. Unfiltered Session pagination retains the existing stable
cursor order; a null v2 timestamp must not be interpreted as that cursor's
legacy placeholder date. Thread Event v2 lists use the same Event time filter;
Thread lists have no time-filter contract and reject an explicit time predicate
with 400 rather than ignoring it.

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

## Runtime semantics and evidence

These Session-scoped rules cross roster admission, committed execution input,
tool realization, and public events. Each claim below names its current owner
and its executed evidence or states the remaining qualification boundary.

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
- **Web domain lists compose at execution, without changing the authored
  singleton shape.** The extension still decodes each Agent's one
  `WebDomainFilter::{Allow, Block}`. `RunActivation` freezes versioned caller
  toolsets outside the publication; a committed `ResumeTicket` retains that same
  input. Coordinated child admission reads the Session's current coordinator
  tools, while ordinary delegation carries the caller and its ancestors. Direct
  and recovered Native children install the same extension-owned effective
  configuration. Every list applies to a URL: allow lists intersect by host/path
  coverage and block lists accumulate. Fetch refuses outside URLs before network
  I/O with `url_not_allowed`; search omits rejected results. An empty combined
  allow region remains a configured tool, reports `url_not_allowed`, and tells
  the model that no domain is permitted. `max_content_tokens` and
  `user_location` take the first whole value stated by the child, then a caller,
  then the coordinator. Existing redirect and provider-server capability fences
  also activate for inherited-only restrictions.

  The `web::configuration::tests` decision table covers parent/subdomain/path
  intersections, mixed allow/block rules, disjoint lists, scalar precedence,
  malformed input and a monotonicity property. The Web tool test witnesses
  provider calls and zero-effect refusals; the child attempt test uses a real
  loopback HTTP request both before and after activation serialization. Session
  coordination tests freeze current settings on the next child admission while
  retaining the previous dispatch. Root/child activations use a closed format
  marker: an old decoder that ignores unknown fields still rejects the marker,
  and an old registered Worker lacks the exact required capability. A selected
  legacy child Web tool without reconstructable caller evidence fails closed.
  ACP and remote child executions that select Web tools remain explicitly
  unavailable until those adapters can enforce the same inherited policy;
  capability selection cannot silently drop it.
  The published Agent's `domains` value and fingerprint are never rewritten to
  store this per-Run policy. Current `web_fetch_text_limit` counts bounded UTF-8
  bytes; exact CMA token accounting remains a separate qualification, not a
  claim inferred from the field name.
- **`agent_with_overrides` propagation.** CMA applies session overrides to the
  coordinator and its `self` copies, never to a roster agent referenced by id.
  `project_effective_agent_publication` and `project_session_tool_override`
  remain the single projector for matching coordinator identity; referenced
  roster snapshots keep their own toolsets. The coordinated child admission
  test pins the non-self case and the current coordinator source. This does not
  constitute cross-product proof of every override field.
- **`user.tool_confirmation` for a non-`ask` event.** CMA rejects it with a 400,
  including for a call denied under `auto`, so a client cannot override a denial.
  The inbound owner is `state/events/admission.rs`, which builds candidates from
  committed pending tickets and calls `resolve_tool_reply` in `recovery.rs`.
  A call with no pending owner is rejected with `RunError::bad_request`.
  R10 in `tool_reply_target_resolution_decision_table` pins the resolver rule.
  Host config H5 uses the real Native Runtime and an injected auto-deny
  evaluator to prove an error result with no tool side effect or pending ticket.
  The official SDK A4 sends a confirmation for a real auto-allowed event and
  observes HTTP 400. The shipped default auto evaluator never emits deny;
  vendor-equivalent risk classification is not established by these tests.
- **Session concurrency and shared budget already have owners.**
  `SessionApplication` reserves 24 ordinary child slots because the public limit
  includes the primary Thread; `coordinated_child_admission` exempts Advisor
  Threads, and `enqueue_session_child` is the atomic dispatch admission owner.
  `coordinated_agent_admission_preserves_activity_across_ambiguous_delivery`
  pins the 24-slot command. The existing
  `shared_budget_child_boundaries_project_with_terminal_priority_live_warm_and_cold`
  table pins shared-budget projection across live, warm and cold views. Runtime
  `DelegationLimits::max_parallel` is a different parent-Run budget and is not
  evidence for this Session limit.

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
