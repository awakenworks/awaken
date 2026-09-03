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
exported as one Session MCP toolset. The reserved `awaken_session` namespace
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
backslashes, empty segments, `.` and `..`. Runtime maps it to either the physical
Workdir root or `/workspace` inside Namespace/Container and sends that value in
ACP `session/new.cwd`. The process cwd remains an implementation detail and does
not grant filesystem authority.

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
