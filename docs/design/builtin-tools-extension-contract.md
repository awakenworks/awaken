# Builtin Tools Extension Contract

This document makes the official builtin tools extension explicit. Runtime core
ships no concrete model-callable tool ids. First-party tool ids come from
`awaken-ext-builtin-tools` and enter runtime through plugin, catalog, permission,
and execution ports.

## Package Boundary

`awaken-ext-builtin-tools` may provide descriptors, schemas, concrete in-process
tool implementations, and integration tests. It does not own runtime core, admin
authority, credential secrets, or public protocol DTOs.

| Toolset | Default role | Execution owner | Notes |
|---|---|---|---|
| `builtin-hand-tools` | local hand operations | Runtime extension (in-process) | `bash`, `read`, `write`, `edit`, `glob`, `grep`, `web_fetch`, `web_search` |
| `builtin-task-tools` | runtime task and recovery helpers | Runtime extension plus Dispatch / Server | `send_message`, `cancel_task`, `recover_failed_messages` |
| `builtin-delegation-tools` | sub-agent invocation | Runtime extension (in-process sub-run) | one `agent_run` tool with `agent_id` argument |

Each toolset is independently enabled. Installing the package does not make every
tool visible to every agent.

## Common Contract

Every builtin tool must define:

- stable descriptor id;
- JSON schema;
- content or implementation fingerprint;
- required capability segment;
- permission policy keys;
- state/effect behavior;
- error result shape;
- replay and audit expectations.

Tool descriptors are fingerprinted when model-visible. Permission still gates
invocation. Execution location still decides where and how the work runs.

## Hand Tools

Hand tools expose filesystem, shell, and network-like capabilities. They execute
in-process within the extension and require a permission policy before use.

Rules:

1. the tool reads its paths from validated arguments; absolute paths are an
   environment detail of the running process, not runtime authority;
2. model-visible paths are logical or workspace-relative according to the
   extension's environment;
3. `bash` requires explicit shell capability and command policy;
4. `write` and `edit` require write permission and conflict handling;
5. `web_fetch` and `web_search` require network policy and source/audit handling;
6. filesystem, shell, and network operations run in-process in the extension; the
   runtime owns execution and commits the result through the normal tool path.

## Task Tools

Task tools are ordinary runtime extension tools over existing state/effect/ingress
ports. They must not create a second task runtime.

`recover_failed_messages` is operations-scoped. It inspects or replays failed
durable messages only when an agent profile explicitly grants recovery authority.
Default business agents should not see it.

## Delegation Tool

`agent_run` is the only first-party delegation tool id. The target agent is an
argument, not part of the tool id.

Invocation rules:

1. hide `agent_run` when the resolved agent has no delegate roster;
2. require `agent_id` to match the resolved delegate roster;
3. include visible target metadata in the descriptor fingerprint;
4. apply permission policy to both the tool id and target `agent_id`;
5. route execution through normal backend/ingress ports.

Do not generate `agent_run_<agent_id>` descriptors.

## First Vertical Slice

1. Register one hand tool descriptor and its in-process implementation.
2. Fingerprint the descriptor in the resolved tool catalog.
3. Gate invocation through permission policy.
4. Execute the tool in-process and produce a `ToolOutput`.
5. Commit tool result and audit facts.
6. Add a negative test proving runtime core has no concrete builtin tool ids.

## Guardrails

G8, G9, G14, G16, and G21 in [INVARIANTS](../INVARIANTS.md). The stable
tool roles remain in [tool-and-capability.md](tool-and-capability.md#tool-and-capability-role-catalog).
