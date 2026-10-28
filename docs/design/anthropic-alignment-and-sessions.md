# Managed Agents Runtime Protocol Adapter

This doc covers the downstream `managed-agents-runtime-protocol` adapter. It is
intentionally not the runtime architecture and it does not include Managed Agents
management APIs. The concrete Dream management API and its Dream
orchestration are owned separately by
[Managed Dream](managed-dream.md).

## Protocol Naming

The public compatibility surface should be named
`managed-agents-runtime-protocol` when it exposes only runtime/session behavior:

- session or thread event ingress;
- run/message submission;
- streaming public runtime events;
- interruption, cancellation, and resume;
- custom/client-executed tool use.

The name must not imply support for agent, model, provider, credential, catalog,
or deployment management. Those capabilities remain config/admin concerns and
require separate contracts if they are ever exposed.

AG-UI is a separate public protocol adapter. It may share neutral runtime ports,
tool descriptors, wait/resume mechanics, event records, and
projection infrastructure, but it must not share Managed Agents DTOs, event
names, beta headers, error schemas, or conformance tests. Public protocol names
stay one adapter at a time; shared behavior lives below them as neutral runtime
and dispatch ports.

## Ownership

| Concern | Owner |
|---|---|
| Public DTOs, beta headers, error schema, event names | Product adapter |
| Runtime facts, `StreamEvent`, `EventRecord`, termination records | Runtime Core |
| HTTP/SSE routing, protocol replay, durable ingress | Dispatch / Server |
| Anthropic Outcome mapping | Product adapter over `awaken-ext-goal` |

## Anti-Corruption Layer

The adapter translates at the edge:

```text
public request/event/status <-> neutral runtime activation/fact/verdict
```

Public names such as `requires_action`, Managed Agents sessions, Anthropic beta
headers, or outcome result enums must not appear in runtime crates. Runtime errors
remain neutral; product adapters decide public error `type` and response shape.

## Sessions

Treat public sessions as product projections over runtime/server state:

- public session id maps to runtime thread/run identity through the adapter;
- identity is not authority;
- resumption behavior is a public protocol contract, not a runtime rule;
- public stream history is derived from committed facts/protocol replay.

Do not add a product `SessionRecord` to runtime core unless the same value object
is required for non-product runtime execution.

The immutable Session mutation policy is internal creation truth, not Managed
Agents wire vocabulary. Ordinary `/v1/sessions` create and update routes still
lower to `Managed`, omit `mutation_policy`, preserve their historical
serialization and fingerprints, and retain the existing Agent/Resource/MCP
authoring behavior. `SessionSystemPromptSelection::{Inherit, Clear, Replace}`
freezes the public request's omitted/explicit-null/exact-value distinction;
`Inherit` remains omitted and legacy-fingerprinted, while GET resolves the
frozen choice against the pinned Agent publication. Private profiled
WorkUnit/Interactive modes never project into Managed request or response DTOs;
their post-create authority is owned by
[ADR-0066](../adr/0066-session-service-binding-and-realization.md#2026-08-27-amendment-immutable-post-create-mutation-authority).

## Environment and local-platform compatibility

Anthropic Managed Agents compatibility ends at the public Agent, Session,
Environment, Resource, event, error, pagination, beta-header, and streaming
contracts. The selected execution provider is private realization detail. A
client therefore uses the same `anthropic-beta: managed-agents-2026-04-01`
request shapes whether the Session runs through Linux bubblewrap, macOS
Seatbelt, the Windows Local provider, or a configured container/Kubernetes
Worker. No provider name, host path, executable path, or fallback flag is added
to the Managed wire.

Static ownership remains one-directional:

| Concern | Authoritative owner | Platform realization |
|---|---|---|
| Agent/Session/Environment DTO and lifecycle | `awaken-protocol-managed` | Identical on Linux, macOS, and Windows |
| Frozen Environment/resource selection | Session projection and resource manifest | Provider-neutral IDs and logical mount paths |
| Execution adapter | Runtime Host `DeploymentConfig` and `SessionEnvironmentProvider` | Linux Namespace, macOS Namespace, Windows Local by default |
| Host programs | Existing Local/Namespace process launcher | Discovered from host execution metadata; never authored by the Managed request |
| Files, Repository, MemoryStore, Skill, output | Existing Resources applications and provider realization | Projected into the Session workspace; never exposed as an arbitrary host path |

The dynamic path is likewise unchanged:

```text
Managed Agent publication
  -> create Session with frozen environment_id and resource bindings
  -> Runtime Host resolves the configured platform adapter once
  -> provider projects resources into the Session workspace
  -> Native or ACP execution launches through that provider
  -> committed runtime facts project back to Managed events/SSE
  -> terminal cleanup harvests outputs and releases the same Environment
```

Windows intentionally defaults to the existing host-backed Local provider
because no Windows Namespace adapter exists. This is an explicit configuration
default, not an implicit downgrade after an isolation request. Explicit
`sandbox_tier = "namespace"` remains fail-closed on unsupported platforms;
shared and multi-tenant deployments must select a container/Kubernetes Worker.
On a trusted workstation, explicitly selecting `sandbox_tier = "local"` on any
OS permits installed ACP CLIs and shell programs to run as host child processes,
while managed resources retain the Session workspace as their logical authority.
The process itself is not confined to that tree. A Namespace/Container Worker
may report the additional exact backend only through
`sandbox_trusted_host_enabled = true`; a Session still has to freeze an explicit
TrustedHost Environment policy before placement can select it.

## Defaults outside the Managed vocabulary

The official Managed Agent DTO has no `max_steps`. Standard creates use the
canonical Runtime default of no additional step ceiling. Native config authoring
may set `max_steps` to a positive integer or explicitly clear it with `null`;
zero is rejected. Managed updates preserve those existing private controls and
Managed responses never add them. Template selection preserves authored limits;
new Console drafts and the administrative drafting tool share the no-ceiling
default. Internal grader/compaction Agents retain their explicit finite limits.

`AgentConfig` and `ResolvedSpec` remain the only configuration and execution
owners. Positive numeric JSON remains byte-identical for old fingerprints;
existing finite publications and Sessions retain their frozen values. Removing a
limit is a new configuration revision/publication, not a reinterpretation of old
data. Snapshot recovery consumes the frozen optional value, never a live default.
Native execution applies its step ceiling only when present. ACP projects that
same optional value into its existing request/tool-call controls; omission removes
only Awaken's additional limit and makes no promise about a CLI's own limits.

Official controls (Session `budget`, Outcome `max_iterations`) retain their own
semantics. Authorization, ownership fencing, cancellation and error recovery are
independent of step limits. No new infinite-run status, metadata convention,
compatibility configuration store, or adapter-specific execution loop is added.
Optional private behavior stays inactive by default; required provider/environment
configuration still resolves through publication and fails explicitly if missing.
Tests own the decision tables for numeric/null/invalid inputs, >16-step execution,
finite exhaustion, cancellation, ACP projection and restart compatibility.

## Managed model and coordinator compatibility

The public model object is translated through the existing provider-neutral
`InferenceOptions` value. `effort`, `speed`, and `inference_geo` are frozen in
the published Agent revision and copied into each Session/thread snapshot. Geo
is a placement constraint, not route identity: an execution adapter must either
prove it can honor the exact value or reject the request before provider network
I/O. A coordinator and all ordinary roster agents must have identical explicit
geo pins, or all omit the pin. This validation belongs to the canonical Agent
publication path, not to a Cloud-only shadow registry.

The Managed compatibility boundary exposes only Anthropic's official
`inference_geo` vocabulary, `us | global`; `global` and omission both mean no
additional caller-selected placement constraint. Provider-specific or
non-Anthropic boundaries must not be added as fields to `model`, Session, or
Agent response DTOs. A hosting adapter may carry an opaque provider placement
proof in the existing resolved candidate and exact-target seams, but clients
cannot select its mechanism or observe Provider routing configuration through
the Managed wire. An internal non-US placement therefore projects as no public
`inference_geo`, while `us` remains portable: every Provider candidate must
prove US processing, and only Anthropic Messages may realize that proof by body
injection.

The versioned `agent_toolset_20260401` has exactly eight built-in names: `bash`,
`read`, `write`, `edit`, `glob`, `grep`, `web_fetch`, and `web_search`.
`web_search` reuses the one WebSearch plugin/provider registry and its existing
credential, egress, usage, and Billing seams; the Managed adapter only selects
it through the official toolset policy.

The Managed protocol's `tools` module owns `AgentTool`, its closed versioned
DTOs and the fallible wire-to-neutral compilation/projection. Control's authoring
HTTP adapter consumes that same module. Session configuration contains only
neutral toolsets and opaque configuration values; it does not decode Managed
Web fields. Every authored duplicate is validated before first-wins folding.
Custom JSON Schema keywords stay open; non-Web and MCP opaque configuration is
preserved rather than interpreted as builtin Web policy.

The existing `awaken-web-policy-contract` SDK owns the pure typed Web kernel:
parse, intrinsic/client-roster validation, normalization, composition and URL
admission. It depends on serde, JSON, URL parsing and field-path decoding, with
no Runtime, HTTP, credential, filesystem or Session application dependency.
`awaken-ext-builtin-tools` retains its existing public Web configuration paths
as real re-exports of those same types. The independent SDK consumer checks
both paths and executes compilation and composition without workspace features.

The existing `awaken-tool-contract` supplies the required
`ToolConfigurationCompiler` constructor role. ConfigService and Session
Application receive the concrete Web extension compiler from their composition
roots. Ordinary and audited authoring writes, validation, preview, publication, effective Session
creation/update and plugin resolution all use the same kernel before credential
or provider effects. Errors carry an owning field path and stable code (for
example `invalid_web_domain_count`), including the original index in legacy
HTTP tool arrays. Compilation returns the canonical value atomically and never
publishes a partially normalized input.
The Web SDK's `WebConfigurationContext` states the boundary explicitly:
`Publication` requires the exact client-tool roster; `Frozen` preserves that
prior roster decision and still checks shape and intrinsic restrictions.
The `parse_in`, `validate_in`, `compile_value_in` and `compile_in` APIs use this
context. Legacy optional-roster entrypoints remain deprecated SDK adapters.
The context decision tables cross publication/frozen input with exact, absent
and case-mismatched rosters, typed/JSON values and malformed intrinsic rules.
They separately prove that decoded configuration grants no fetch without the
required URL evidence; a compile-fail doctest rejects a roster-less publication.
 The compiler validates disabled and
later duplicate entries as well as the selected entry.

The extension retains `WebConfigurationIssue` (field path, stable code and
message) through frozen decoding and inherited composition. Existing SDK
String-returning helpers and plugin builders format it only at their compatibility
boundary; they do not maintain a second parser or silently drop invalid sources.

Web execution settings remain in the neutral
`ToolPolicyOverride.configuration` value, separately from provider bindings and
options in `plugin_config`. Installed provider capabilities and prepared
configuration do not authorize an individual fetch. The Web extension checks
the requested URL against every frozen source and domain restriction before
resolving credentials or dispatching provider I/O. Supporting providers must
enforce domains on every redirect hop or disable redirects; unsupported
combinations fail during assembly. Fallback remains limited to
`UnavailableBeforeDispatch`.

Every host fetch validates an HTTP(S) URL with a host before credential delivery,
including requests with no additional execution policy.

URL evidence is a versioned, rebuildable view of the committed conversation,
not another persistent authorization authority. User messages, matched tool
calls/results, and frozen descriptor ownership supply its facts. A name alone
cannot classify a third-party or client tool as an intrinsic Web capability.
Thread State reading, transcript observation, and pure request admission are
separate operations; missing required evidence or an unsupported evidence
format refuses the fetch. The private admitted request only enforces ordering
inside the process and carries no cross-process authorization.

The existing `AfterTool` outcome captures the original frozen tool ownership and
an equality fingerprint bound to its Run, successful call and output. The same
derived State key holds these witnesses. The existing phase SPI's optional
`on_model_input` observation reports the materialized input of a successful
model request after context
selection. Web promotes URLs only from uniquely matched conversation messages
in that input and, for tool results, their original executed witness. Hidden,
ambiguous or mismatched results and historical results without their witness
contribute no tool-source evidence. Internal delegated task/advisor inputs are
model-authored transport messages even when their role is User; their existing
message identity excludes them from user URL evidence. Recovery cannot classify
historical results from the current descriptor catalog. An outcome alone cannot
authorize a fetch before that result reaches an observed model input.
Client-executed result content cannot carry Runtime state-writer authority:
the resume boundary rejects nonempty client state commands before consuming
the ticket or running hooks. Trusted in-process tools retain their state ports.

`max_content_tokens` is a lightweight estimate for host-returned text, not a
model-specific tokenizer count: four ASCII scalars or one non-ASCII scalar cost
one estimated token; fractional ASCII costs accumulate across the prefix.
Truncation preserves a valid UTF-8 scalar prefix. It does not promise grapheme
boundaries or a hard model-context ceiling. The nearest configured value still
wins (including zero); absence does not introduce a content cap. This changes
the earlier byte-as-token implementation, so the same configured number can now
retain more text. All returned Strings use this budget, including text returned
from PDF URLs; the extension does not claim to parse binary documents.

Three distinct owners remain: each HTTP adapter bounds transport bytes (the
direct adapter retains its 1 MiB read fence), the Web extension projects returned
text using the estimated budget, and existing model usage handling records the
provider's reported request totals. Request usage includes other messages and
cannot be attributed to one fetched page or retroactively protect a request
that has already been sent. Provider-server options remain that provider's own
capability; a host budget that cannot be enforced by that route is refused.
The native test matrix crosses Unicode/zero/huge budgets, absent and nearest
inherited preferences, PDF text, fallback timing and a real oversized loopback
response. Source and domain restrictions continue to be conjunctive authority
checks before credentials or I/O; content preferences cannot relax them.

`glob` and `grep` share one capability-relative walker rooted in the
`ConfinedPath` directory handle. The compatibility floor follows the published
Go toolset: Glob applies `*`, `?`, character classes, and segment-level `**` to
all real files and directories, including entries below `.git` and
`node_modules`, visits at most 50,000 entries, returns at most 200 matches, and
orders newest first. Grep alone prunes `.git`, `node_modules`, hidden and ignored
paths, skips symlinks, binary inputs, and files above 8 MiB, and caps output at
100 KiB with the exact `[output truncated]` notice. Its deterministic in-process
path intentionally implements the ordinary ripgrep-visible behavior without
making compatibility depend on an executable found on host `PATH`.

Trusted absolute patterns below the Workdir or a configured logical projection,
brace alternatives, and `@(...)` alternatives are Awaken-compatible extensions;
they cannot broaden the opened capability and are tested separately from the
required compatibility rules. The logical output base is frozen when the
directory capability is opened. Traversal and rendering never canonicalize an
ambient pathname again, so a later projection-path swap can neither redirect
bytes nor disclose the provider's physical host path.

Multiagent authoring and execution continue through the one `AgentConfig` roster
and Session thread/event authority. The Managed roster admits only Agent
references and the official self reference. Internal advisor targets remain a
native control-plane capability and are neither accepted nor projected as a
private Managed union member.

Public Session status is restricted to the official
`rescheduling | running | idle | terminated` union. Provisioning and internal
failure phases remain internal state and are projected to the nearest official
observable state/event rather than leaking adapter-private enum members.

## MCP URL compatibility and sandbox stdio

The Managed wire accepts the official URL MCP shape. `sandbox_stdio` already has
one authoritative internal path—Agent binding, Session attachment/generation,
Environment process realization, and the runtime stdio MCP client—but a command
and arguments cannot be represented as a Managed URL. The Managed DTO therefore
has no stdio variant; native bindings are omitted rather than assigned a
fabricated URL.

The standards-preserving target design is:

| Classification | Authority | Role |
|---|---|---|
| Reuse unchanged | `McpTarget::SandboxStdio`, Session MCP generation/lease, Environment process realization, `awaken-ext-mcp` stdio client | Own the command, process, connection, and exact Session lifetime |
| Modify | Runtime Host `McpRelay` exact-generation capability table | Add a stdio upstream target beside its existing HTTP upstream target; keep one route/lease/cleanup authority |
| Modify | Managed Session MCP projection | Project the realized, capability-bearing bridge URL as the ordinary `{type:"url"}` shape |
| New | stdio request/notification adapter inside the relay | Translate Streamable HTTP requests and session lifecycle to the existing stdio MCP connection without building a second tool registry |
| New | native registration command, if callers must author commands | Register a sandbox command outside the Managed wire and return an opaque MCP reference; credentials remain typed bindings, never URL data |

Static structure:

```text
native Agent MCP binding (sandbox command)
  -> Session MCP attachment + exact generation/lease
  -> Session Environment process + existing stdio MCP connection
  -> existing McpRelay route table (stdio upstream variant)
  -> session-scoped capability URL
  -> standard Managed {type:"url", name, url} projection
```

Dynamic behavior:

```text
Session realization starts the frozen command
  -> initialize the stdio MCP peer
  -> atomically stage the exact-generation relay route
  -> expose the URL only after both process and route are healthy
  -> forward POST/GET/DELETE and notifications through that connection
  -> reject unknown, stale, expired, or cross-Session capabilities
  -> retry by creating a new generation and URL, never by retargeting an old one
  -> terminal Session cleanup removes the route and reaps the process
```

Until that bridge exists, native execution may consume sandbox stdio directly.
It must not fabricate a URL, expose command data through Managed fields, or
silently reinterpret a URL request as a local process.

## Events

Runtime emits neutral facts/events. The product adapter projects them:

- after commit;
- with public names and public status mapping;
- with product-specific exactly-once or replay rules;
- with no mutation of runtime truth.

Mid-run waits are represented by the neutral lifecycle/commit path and translated
to the product's public wait shape at the adapter.

## Custom Tool Use

Managed Agents custom tool use is a runtime wait/resume channel, not a
management API. The compatible slice maps a client-executed tool call through the
adapter:

```text
neutral tool request
  -> agent.custom_tool_use { id, name, input, session_thread_id? }
  -> user.custom_tool_result { custom_tool_use_id, content, is_error? }
  -> neutral tool result / run resume
```

The public `id` is the correlation and Session Thread routing key. It maps to the
neutral pending tool call id used to resume execution. Public `name` and `input` map to the selected
tool descriptor and arguments. Capability, deadline, fingerprint, permission, and
policy metadata are derived from resolved config and runtime context; they are
not accepted from the public event and are not projected back onto the public
wire.

Frontend or client-executed tools reuse this same public channel. Do not add
adapter-specific `agent.frontend_tool_use` or `user.frontend_tool_result` event
types. The distinction between custom, frontend, remote, or builtin tool sources
belongs to descriptor/catalog metadata before projection, not to runtime core.

Inbound `user.custom_tool_result` must fail closed when the referenced pending
tool call is unknown, already answered, expired, mismatched by thread/session, or
not owned by a client-executed tool. Successful results and error results are
both normalized into the same neutral result path before the runtime continues.
For multiagent replies, the qualified public tool-use event id is the routing
authority; the SDK input does not accept a separate Session Thread selector.
Outbound events may echo `session_thread_id`, but clients reply with the event
id. Legacy unqualified ids are accepted only when the committed pending call and
reply kind identify exactly one Session Thread. Validation carries that one
resolved target through activity admission and execution rather than resolving
again or keeping a protocol-side tool registry.

This support does not imply support for Managed Agents management endpoints.
Clients may provide per-run client tool descriptors when an adapter explicitly
allows that runtime option, but they may not create, update, publish, or delete
agent specs, model/provider bindings, credentials, or global tool catalog
entries through the custom-tool channel.

## Outcome / Goal Mapping

Anthropic Outcome is a product specialization:

| Product concept | Neutral mapping |
|---|---|
| `user.define_outcome` | command adapted to `outcome::Definition` and the Outcome Runtime Extension |
| outcome evaluation span | projection of durable Outcome Evaluation records |
| `satisfied`, `needs_revision`, etc. | product mapping of `outcome::EvaluationResult` |
| max iterations | Outcome extension policy; an explicitly configured Runtime Step ceiling is independent |

The Outcome Extension owns Iterations, Worker/Grader Runs, and recovery over
durable Worker Thread state. Runtime Core records neutral Run facts; the product
adapter owns public outcome names, wire compatibility, and external-runtime
input projection. Neither Runtime Core nor ACP learns Outcome vocabulary.

## First Vertical Slice

For a Managed Agents-compatible product slice:

1. define one public request DTO;
2. map it to one neutral runtime activation or Outcome extension command;
3. execute through server/runtime ports;
4. project one committed fact to one public event;
5. add snapshot tests proving no public names leak into runtime errors.

## Non-Goals

- No Anthropic DTOs in runtime crates.
- No product session model as runtime truth.
- No public event emitted before the runtime commit.
- No broad Managed Agents product crate family beyond concrete adapter slices;
  Dream is one such product slice and retains its own owner document.

## Guardrails

G1, G10, and G11 in [INVARIANTS](../INVARIANTS.md).
