# ACP Harness capability, tool bridge, and approval contract

## Decision

An Agent runtime selects who owns the model/tool loop; it does not implicitly
grant every Awaken feature. Publication and the Console use the same runtime
capability projection. Unsupported configuration is disabled in the Console and
rejected by the compiler.

Awaken keeps the Anthropic Managed Agents toolset wire (`agent_toolset_20260401`)
and its canonical member names. Execution ownership is selected exactly once:

| Capability | Native Awaken | ACP Harness |
| --- | --- | --- |
| read/write/edit/bash/glob/grep | Runtime `RawTool` | Harness builtin only when certified equivalent; otherwise Awaken Session MCP bridge |
| WebSearch/WebFetch | configured Awaken provider/host implementation | one selected provider-server implementation or Awaken Session MCP bridge |
| Skill | prepared instructions plus the semantic Skill entry | the same prepared instructions/entry projected to ACP; no duplicate harness tool |
| Memory | durable Awaken store and semantic tools | prepared recall plus bridged semantic tools; ACP local memory is not authoritative |
| State machine | supported | unavailable: the external Harness owns the model/tool loop |
| Background tool execution | supported | unavailable until ACP has a durable detached-tool contract |
| Working directory | Session workspace root | optional portable subdirectory mapped onto the same Session workspace |

OpenAI/Anthropic API compatibility describes an inference wire, not WebSearch,
WebFetch, filesystem, Skill, or Memory execution. Provider-server tools are
enabled only by an explicit provider + API dialect + adapter realization. A
DeepSeek or OpenRouter-compatible endpoint therefore receives no builtin tool
assumption from its branding alone.

## Before and after

Before, an ACP publication could list `write`, while runtime exported only Web,
Skill, and Memory tools. `mcp.awaken_session.write` would also normalize to the
external MCP identity `mcp__awaken_session__write`, so a `write` permission rule
could not match. State-machine configuration remained visible even though ACP
never ran the Native plugin loop. ACP cwd was fixed to the workspace root.

After, the immutable publication selects the exact Hand descriptor subset and
the bound Session environment supplies the matching rooted executors. They are
exported as one Session MCP toolset. `awaken-runtime-contract::tool_export`
owns the export port/lease; `awaken-protocol-mcp::SessionToolExporter` owns the
single transport adapter. Both the Coordinator Host and registered Worker Host
install that adapter, including AllInOne's registered Worker. The Host retains
permission grants and binds executors only after Environment realization. The reserved `awaken_session` namespace
maps back to canonical IDs such as `write`; third-party MCP namespaces retain
`mcp__server__tool`. State machine and background execution are both Native-only
at authoring and publication. ACP supports a validated workspace-relative cwd.

## Approval and replay safety

The expected write path is:

`ACP permission ask -> Awaken policy -> durable Await ticket -> human decision
-> ACP resume -> one-shot execution grant -> Session MCP tool -> rooted RawTool
-> Artifact/Trace -> completion`.

The exported MCP tool is guarded independently. `Always allow` may execute
directly. `Always ask` or deny cannot cross the dispatch boundary without the
one-shot grant observed from the ACP permission resolver. A grant matches
canonical tool ID plus arguments and is consumed once. It is deliberately not
durable: the approval ticket is durable truth and a recovered ACP process asks
again, receiving a new one-shot grant only after the stored decision is applied.

## Working directory

`configuration.working_directory` is optional and relative to the Session
workspace. Publication rejects empty paths, absolute paths, drive/prefix syntax,
backslashes, empty segments, `.` and `..`. Runtime derives one filesystem view:
the default is `/workspace`, and an authored value extends it. Namespace and
Container pass the sandbox-absolute value to both the process and ACP
`session/new.cwd`; Workdir passes the logical value to the process launcher and
the matching platform-native host path to ACP. Resource prompt paths are made
relative to that same cwd for ACP, so no private Workdir root is injected.
Native Hand tools retain provider-neutral sandbox-absolute paths because their
path jail interprets the logical sandbox contract rather than a host process
cwd. A TrustedHost fallback pool cannot mix these two path consumers in one
attempt; Namespace/Container may mix them because both consume the same
sandbox-absolute view. The process cwd does not grant filesystem authority.

Namespace projects an explicitly selected npm entrypoint together with its
outer `node_modules` dependency tree, read-only. Linux bind rendering and macOS
read grants share the runtime-root selector. The installation parent is not a
runtime grant. Protocol negotiation on the host alone cannot certify that the
entrypoint and its imports are reachable inside an execution Environment.

## Diagnostics

Errors are classified at the failing boundary:

- `AGENT_TOOL_NOT_ENABLED`: the immutable publication did not select the tool.
- `ACP_TOOL_BRIDGE_MISSING`: selected tool descriptors cannot be paired with a
  Session executor/export adapter.
- `ACP_TOOL_UNSUPPORTED`: the negotiated Harness cannot consume the selected
  projection.
- `TOOL_PERMISSION_BLOCKED`: policy denied before dispatch.
- `TOOL_APPROVAL_REQUIRED`: the ACP client attempted direct MCP dispatch without
  completing the permission request.
- durable Session Await represents `TOOL_APPROVAL_PENDING`.
- `TOOL_EXECUTION_FAILED`: dispatch occurred and the underlying tool failed.
- `PROVIDER_SERVER_TOOL_APPROVAL_UNSUPPORTED`: the provider owns execution, so
  Awaken cannot pause that call for per-call HITL; choose the Awaken-hosted
  realization or `always_allow`.

## Required verification matrix

For each supported ACP adapter (Codex, Claude, Gemini), test discovery and the
exact received tool list; read success and path denial; write AlwaysAllow;
write AlwaysAsk approve, deny, refresh, restart, duplicate approval, and changed
arguments; Artifact/Chat/Trace consistency; missing exporter and unsupported
MCP transport; cwd root, valid subdirectory, traversal, absolute path, and
Namespace/Container mapping. Provider tests cover host WebSearch, certified
provider-server WebSearch, unsupported compatible APIs, and mutual exclusion.

Console tests cover runtime switching, disabled Native-only configuration,
preservation of existing published revisions, runtime installation/login/version
diagnostics, and publication error rendering. The hermetic product E2E uses a
deterministic ACP process but the production Runtime Host, Session MCP exporter,
rooted tools, durable approval store, restart path, and Files projection. A
separate live canary certifies each real ACP CLI; the deterministic fixture does
not substitute for that adapter certification.

## Claude SDK lookup aliases

The pinned Claude ACP 0.69.0 adapter consumes SDK 0.3.232 `toolAliases` through
`session/new._meta.claudeCode.options`; the same metadata accompanies load and
missing-session recovery. This is a catalog-declared adapter extension, not an
ACP-wide option or an arbitrary user-authored SDK options object.

The canonical ACP catalog declares `supports_tool_aliases`. Control freezes it
in the existing `AcpExecutionProfile`; publication checks every candidate. Old
profiles default to unsupported. The capabilities endpoint reports each facet:
`tool_aliases`, `tool_alias_semantics`, `tool_description_override`, and
`tool_discovery`. The Console disables controls without supported evidence and
keeps invalid retained intent removable. Provider-routed ACP and backend-owned
ACP use the same runtime identity for this gate.

Agent intent stays in `tool_overrides`. Native keeps presentation semantics.
Claude projects `target=write, alias=Write` to
`{Write: "mcp__awaken_session__write"}`; external canonical MCP targets retain their
namespace. MCP descriptors and parameter schemas stay canonical. Alias mappings
are single-hop lookup redirects, not schema adapters. Duplicate source names,
shadowed targets/chains and unsupported description/exposure/discovery settings
fail closed. Unsupported ACP rows cannot publish aliases.

A nonempty alias projection disables the SDK builtin set (`tools: []`) and
passes alias sources through `disallowedTools`, preventing builtin-object calls
from being mistaken for governed execution. Other exported MCP tools still obey
their own declared policies. The runtime refuses delivery when its catalog row
cannot support the mapping. The canonical permission normalizer accepts both
`mcp.awaken_session.write` and Claude's `mcp__awaken_session__write`; both resolve
to `write` before permission evaluation and one-shot execution grants.

Neither aliases nor tool permissions implement filesystem isolation. A Bash
command can mutate files without calling Write; Environment capability is the
boundary. Unsupported isolation cannot silently fall back to Workdir.

Verification is split deliberately: protocol tests cover new/load/fallback and
metadata; compiler tests cover capability admission; the existing tool-bridge
E2E covers real governed tools, durable approval, restart and artifacts. The
pinned SDK and actual ACP adapter tests use a shared loopback inference fixture
to force an alias-emitted tool use and prove real name resolution. They require
no external account and do not certify live model task quality.

Cowork is an API application built on these mechanisms. It owns task entry and
delivery UX; Agent publication, Session truth, tools, resource admission,
approvals, scheduling and Artifact remain Awaken authorities.

The ACP await transaction also writes the existing `ActiveToolBatch` with the
exact canonical ticket identity and arguments; terminal continuation clears the
batch together with the ticket. Session admission keeps its shared strict
batch/ticket validation. There is no ACP-specific approval bypass. Legacy wire
names are normalized through the same function for decision matching and the
committed transcript; changed arguments and consumed decisions still fail closed.

A permission request owns its complete arguments. Earlier streaming tool-call
notifications may contain empty or partial input and cannot override them. An
exact observed call supplies identity and, only when request input is absent or
null, fallback arguments. Before the Await commit, the current attempt refines
its staged ToolUse to the same input as the resume ticket. A committed prefix
is immutable; conflicting reuse of an already committed call id is rejected.

The same response pump projects `tool_call_update.rawInput` as refinement of
its exact previously observed call before projecting any terminal result in that
frame. Progress titles do not rename tool authority. The existing attempt-local
appender refines one staged ToolUse, including AlwaysAllow calls. On permission
resume, exact regenerated arguments remove only the new staged placeholder and
correlate the result to the original committed call once. Changed arguments after
matching or after a result fail closed; there is no transcript repair lane.

Terminal ACP tool status is committed through the existing neutral
`ContentBlock::ToolResult.is_error`, exactly as in Native. The appender preserves
content and the original resumed call identity; it does not encode failure as a
synthetic text prefix. Session projection and UI consume that one typed fact.
Historical committed prefixes remain immutable and are not repaired by guessing
from error-looking text.

MCP invocation IDs use the shared process-scoped fallback identity generator.
An export can be reconstructed while its Environment executor still retains an
unresolved Hand call; a per-export counter would alias independent calls across
that boundary. Fresh invocation identities do not authorize replay of an old
effect or replace Runtime-owned durable operation tokens. The relay still keeps
the unresolved old operation indeterminate and rejects a distinct unsent call
through the existing pre-dispatch replacement path.

A loaded ACP session may remember a permission-boundary request as cancelled.
The continuation therefore carries the immutable ticket's exact arguments and
asks the agent to reissue that request after approval, or refrain after denial.
The existing one-shot resolver still requires exact tool and argument equality;
this prompt does not grant a different command or establish that execution has
already occurred. Real SDK continuation remains a separate acceptance boundary
from deterministic JSON-RPC resume tests.

Physical Hand exports are created only after Environment binding, never during
deferred reservation. The MCP `RawTool` port delegates to
`Environment::tool_executor()`, preserving Workdir rooting and Namespace/Container
resident Hand execution. It does not instantiate a second registry or dispatch
host filesystem implementations on behalf of an isolated Environment.

Reproduce the focused verification with `npm run test:claude-aliases` and
`npm run test:cowork-topologies` from `e2e`, under the existing provider-environment
sanitizer. The topology suite reuses the canonical bridge, Worker transport,
Worker recovery and projected Namespace/Container scenarios. HTTP/NATS variants
of `ConnectionPlan` are rejected by its contract tests; these are distinct from
the implemented Worker HTTP API.
