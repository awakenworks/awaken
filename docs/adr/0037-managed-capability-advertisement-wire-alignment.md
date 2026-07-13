# ADR-0037: Managed Capability Advertisement — Align the Session Agent to the Official Wire

- Status: Accepted
- Date: 2026-07-02
- Relates to: [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (managed
  protocol is a front-door axis, distinct from the environment/provider axis),
  [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) D7 (a
  managed agent = managed protocol front door + managed Environment),
  [ADR-0036](0036-skills-as-runtime-extension-single-tool.md) (skills are two
  runtime-ext tools), G16 (only the managed crate names Anthropic vocabulary)

## Context

`awaken-protocol-managed` is the anti-corruption adapter (ACL) between the public
Anthropic **Managed Agents** wire (beta `managed-agents-2026-04-01`) and the
neutral runtime. When a caller does `POST /v1/sessions`, the response carries an
`agent` object that is supposed to enumerate what the session can actually do:
its tools, skills, delegate roster, MCP servers, and the session's resources.

The first cut of this enumeration **guessed** the wire shapes. It emitted each
built-in tool as its own `{name, description, input_schema}` definition and each
resource as `{id, type:"file", path}`. Both are wrong against the real SDK:

- The official protocol bundles the built-in tools as **one versioned toolset**
  (`agent_toolset_20260401`), referenced once with per-tool `configs`, not N
  standalone definitions.
- A resource on the wire is a Files-API-backed reference (`sesrsc_…` id +
  `file_id`); awaken has no Files API yet, so any resource object it emitted was
  fabricated.

The governing constraint for this work was explicit: **never guess a wire shape;
align against the official SDK before committing.** This ADR records the aligned
Stage-1 shape and, just as importantly, what is deliberately left empty because
no real producer exists yet.

## Decision

### D1: Built-in tools fold into one `agent_toolset_20260401` reference

awaken's registered built-in (hand) tool ids — `bash/read/write/edit/glob/grep`
(plus the toolset's `web_fetch/web_search`) — are byte-identical to the tools the
versioned toolset bundles. So the adapter advertises them as a **single**
`{type:"agent_toolset_20260401"}` entry, never per-tool definitions. Its
`configs` array encodes the host's real permission reality, not a guess:

- a toolset tool the host does **not** register → `{name, enabled:false}`
  (awaken registers neither `web_fetch` nor `web_search`);
- a registered tool whose calls are **confirmation-gated** →
  `{name, permission_policy:{type:"always_ask"}}` (`bash/write/edit`);
- a registered, auto-allowed tool → **omitted** from `configs` (it takes the
  toolset default; `read/glob/grep`).

`configs` is omitted entirely when empty. Wire shaping lives in
`project::agent_tools`; `state.rs` only supplies neutral `AgentCapabilities` data.

### D2: The permission gate and the advertisement are single-sourced

The `always_ask`/omit split in D1 must equal what the permission gate actually
does at call time, or the agent object lies. `awaken-server-local` derives both
from one list, `AUTO_ALLOWED_HAND_TOOLS = [read, glob, grep]`: `server_policy()`
builds the auto-allow rules from it, and `builtin_hand_tools()` sets each tool's
`ask = !AUTO_ALLOWED_HAND_TOOLS.contains(id)` from the same list. Gate and
advertisement cannot drift.

### D3: Client tools are `custom`; skills and delegates get first-class fields

- A client-executed tool → `{type:"custom", name, description, input_schema}`,
  listed alongside the toolset reference in `agent.tools`.
- Offered skills → `agent.skills: [{type:"custom", skill_id, version:"latest"}]`
  (a new `SessionAgent.skills` DTO field). `custom` because the host offers them
  locally (ADR-0036), not from the hosted Skills API.
- A delegate roster → `agent.multiagent: {type:"coordinator", agents:[…]}` (a new
  `SessionAgent.multiagent` DTO field, `skip_serializing_if none` so a
  non-delegating agent emits no key).

### D4: Absent producers advertise empty, never fabricated

Two wire fields have **no** real producer in the host yet, so they are honestly
empty rather than guessed:

- `agent.mcp_servers: []` — the host wires no MCP tool set.
- `session.resources: []` — a real resource needs a Files-API `file_id` +
  `sesrsc_…` id (ADR-0035 D1/D8 provisioning). awaken has neither, so the earlier
  `{id, type:"file", path}` shape was withdrawn along with the host's
  `with_resources`/sandbox-mount broadcast and the `ResourceMount` re-export. The
  sandbox's own `Mount::Resource` primitive is untouched — only its misuse on the
  managed wire is removed.

An empty field the SDK can deserialize is honest; a fabricated object is a lie the
SDK might place wrong. When a producer lands (Files API, MCP wiring), these fields
get a real shape under a follow-on ADR.

### D5: `outcome_evaluations` carries the documented pair only

The durable `session.outcome_evaluations` entry is `{outcome_id, result}` — the
two documented fields — not the fuller `{outcome_id, iteration, result,
explanation}` shape of the transient `span.outcome_evaluation_*` events. The
session object reflects the outcomes that graded it; the per-round detail stays on
the span events.

### D6: The ACL stays pure — neutral data in, wire shapes out

`AgentCapabilities` (`builtin_tools`, `custom_tools`, `skills`, `delegates`) is
neutral host data with no Anthropic vocabulary; all Managed vocabulary
(`agent_toolset_20260401`, `custom`, `coordinator`) is confined to
`crate::project` (G16). `state.rs` calls `project::agent_tools/agent_skills/
agent_multiagent` at session creation and never names a wire type itself. The
crate still depends only on `awaken-agent-contract`, not the runtime contract.

## Consequences

- The created session's `agent` object matches the official wire: verified by
  golden Rust tests over both fakes (`awaken-protocol-managed`) and the real host
  routers (`awaken-server-local`), and end-to-end by the official
  `@anthropic-ai/sdk@0.105.0` (`e2e/managed_capabilities_e2e.mjs`, wired into
  `npm test`) — the SDK deserializes the session, so a shape it can't place fails.
- Gate/advertisement drift is structurally impossible (D2, one source list).
- No fabricated capability ships: empty fields are honest placeholders with a
  named unblock (Files API / MCP), not silent guesses.

## Non-Goals / Deferred (Part B)

- **Real resource injection.** Files API (`upload → file_id`), a `resources` input
  channel on `CreateSession`, host mount resolution, `sesrsc_…` ids, the
  `sessions.resources` endpoints, and output capture. Until then `resources` is
  `[]` (D4). This is the largest honest gap and warrants its own ADR under the
  ADR-0035 provisioning seam.
- **MCP wiring** into the managed host (`mcp_servers` + `mcp_toolset`).
- **A per-agent capability catalog / `Agent` aggregate.** Today the host reports
  one process-wide capability surface; per-agent capability resolution is future
  work.

All three keep the standing rule: align against the SDK before shipping a shape;
until then, advertise empty.

## References

- [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) — managed protocol
  front door vs environment/provider axis.
- [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) — D1/D7/D8
  provisioning seam; resources are realized here, not fabricated on the wire.
- [ADR-0036](0036-skills-as-runtime-extension-single-tool.md) — skills as
  runtime-ext tools; `custom` skill reference on the wire.
- [INVARIANTS.md](../INVARIANTS.md) — G16 (only the managed crate names Anthropic
  vocabulary).
