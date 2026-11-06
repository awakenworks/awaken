# ADR-0079: One Tool-Authorization Vocabulary and Opaque Backend Configuration

- Status: Accepted
- Date: 2026-09-12
- Supersedes the mode axis introduced with: ADR-0030 permission policy axis
- Preserves: ADR-0037 managed capability advertisement, ADR-0069 ACP capability
  configuration lifecycle
- Amended: 2026-10-09 (reconciled with ADR-0076: the baseline also admits
  delegation and background-task management; the `auto` ordering is decided)

## Context

Tool authorization had accumulated two authoring vocabularies for one decision.
The Claude-Code-derived `plugin_config.permission` section (an ordered rule list
plus a four-valued `mode`) arrived first as the concrete policy behind the
permission axis. The typed Agent/MCP toolsets (`default_config` / `configs`,
Managed Agents wire parity) arrived later and were *compiled into* that older
vocabulary rather than replacing it, so one evaluator served two authoring
shapes. A third, unwritten default sat underneath both: a hard-coded baseline
allow list plus a set of pre-authorized ids.

The config plane had already closed half the duplication — a mutable write
carrying `plugin_config.permission` is rejected, and declaring toolsets beside it
is rejected as "competing permission sources" — but the runtime, the advertised
schema, and the defaults still carried the older shape.

Separately, a leaf backend (an ACP CLI today) reports its own session modes and
config options. Nothing prevented the authorization core from growing a mapping
table over those values, which would make every new CLI a maintenance obligation
and let an unrecognized value change a decision silently.

## Decision

**One vocabulary.** Tool authorization is authored as a per-toolset default plus
per-tool overrides, with ordered pattern rules as the documented extension for
what that shape cannot express (argument-scoped matching and absolute deny).
`PermissionRuleset` carries exactly one ambient posture, `default_behavior`; a
toolset's own default compiles into it.

**No second axis.** The permission `mode` is retired from the authoring surface.
A historical `plugin_config.permission` section still parses so an immutable
publication keeps its execution contract, and its mode folds into
`default_behavior` at parse time: `plan` compiles to deny, `bypassPermissions`
to allow, and the remaining modes preserve the authored default unchanged.
Nothing short-circuits the rule scan any more, so a matching deny is absolute in
every configuration — the retired bypass posture could previously skip the scan
entirely and neutralize a deny, and losing that is a narrowing, never a widening.

**One default, fail-closed.** The built-in baseline grants perception only. It
never grants a controlled modification, and it never grants an MCP tool: an MCP
tool's authority comes from its `mcp_toolset` policy, whose default is ask. Only
the read-only management tools are pre-authorized outright.

**Backend configuration is relay data.** A leaf backend's session modes and
config options are reported as capabilities and passed through as opaque
identifiers. The core never reads their meaning: crates that decide
authorization may neither depend on a backend protocol crate nor name a backend
mode identifier, and both halves are enforced mechanically.

## Consequences

Native, ACP-CLI, and Managed worker execution resolve one compiled authorization
value, so no backend owns a second evaluator, a second default, or a private
escape hatch. Removing the bypass short-circuit removes the only code path that
could render an authored deny inert.

Two items are deliberately left open because each needs a semantic decision
rather than an implementation:

- **Denied calls are not yet visible on the Managed wire.** `Fact::ToolCall` is
  derived purely by folding committed messages, so a policy denial — committed
  today as an error tool result — folds to the executed disposition and projects
  as `evaluated_permission: "allow"`. Reporting it faithfully requires the
  denial to become typed committed truth, which changes the committed fact
  vocabulary and its durable projections.
- **`auto` needs an ordering decision first.** The API response projects back
  from the neutral `ToolsetPolicy`, so that neutral form is the round-trip
  carrier and an `auto` posture must live there rather than being resolved at
  ingest. Admission compares a requested policy against the authored one and
  rejects anything weaker; `auto` is neither weaker nor stronger than ask, so
  that comparison needs a defined answer before the variant is added.

## Amendment 2026-10-09: reconciled with ADR-0076

"Only the read-only management tools are pre-authorized outright" is no longer the
whole list. The 2026-09-13 amendment of
[ADR-0076](0076-session-owned-background-tool-execution.md) admits all four
background-task management tools (`run_in_background`, `list_background_tasks`,
`get_background_task`, `cancel_background_task`); the target tool is still
authorized on its own. The baseline also allows `agent_run` (delegation), the
advisor tool, `list_skills`, `Skill`, `list_memories` and `read_memory`
(`crates/server/awaken-runtime-host/src/config.rs:59-102`). It still never grants a
controlled-modification member of the Agent toolset or an MCP tool.

The open item "`auto` needs an ordering decision first" is closed in code: `auto`
sits strictly between ask and allow on the `GrantedAutonomy` axis
(`crates/sdk/contracts/awaken-agent-contract/src/toolset.rs:95-145`), and
`AgentToolPermissionPolicy::Auto` exists
(`crates/platform/session/awaken-session-configuration-contract/src/agent_config.rs:75`).

## Amendment 2026-10-09: names and references

- Header "the mode axis introduced with ADR-0030": ADR-0030 itself defines no
  `mode`. The four-valued `mode` belonged to the Claude-Code-style rule policy that
  [ADR-0030](0030-permission-policy-axis.md) places in `awaken-ext-permission`
  (its Consequences). This ADR retires that `mode`; ADR-0030's gate and verdicts stand.
