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
| `builtin-hand-tools` | local hand operations | Runtime extension (in-process) | `bash`, `read`, `write`, `edit`, `glob`, `grep`; the two Web tools use the shared configurable Web provider catalog |
| `builtin-task-tools` | runtime task and recovery helpers | Runtime extension plus Dispatch / Server | `send_message`, `cancel_task`, `recover_failed_messages` |
| `builtin-delegation-tools` | Agent delegation | Runtime extension (local or remote child Run) | one `agent_run` tool with `agent_id` argument |

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
5. `web_fetch` and `web_search` are stable builtin identities over one shared
   Web provider catalog. Each tool has its own typed request/result contract and
   route plan, while provider accounts, exact credentials, ordered fallback,
   funding and diagnostics remain configuration-plane facts;
6. filesystem, shell, and network operations run in-process in the extension; the
   runtime owns execution and commits the result through the normal tool path.

### Web provider realizations

The Web provider catalog is one discovery and validation authority with separate
`WebSearchProvider` and `WebFetchProvider` ports. A provider may implement either
or both ports and a Workspace may bind several accounts for the same provider.
Search and Fetch route plans select one primary target and explicit ordered
fallbacks. They never infer a fallback across credential custody or funding.

Gateway-routed providers share one infrastructure adapter for both local and
hosted compositions. The adapter receives a `ManagedWebRouteResolver`; the
resolver alone exchanges trusted Runtime operation coordinates for an exact,
short-lived route capability. The adapter never receives Provider credentials,
never lists Cloud-internal accounts, and never owns entitlement or billing.
Hosted and local Cloud use different resolver implementations over the same
adapter, so they cannot drift into parallel WebSearch/WebFetch execution paths.
The resulting provider descriptor is registered into this same catalog only
after its route is discoverable.

A resolved target has exactly one realization for one model attempt:

- `HostExecuted` invokes a configured provider through the ordinary permission
  gate and `RawTool` path;
- `ProviderServer` projects the existing builtin identity into a model-gateway
  server-tool representation. The provider adapter owns the wire spelling and
  must normalize results and usage back to the builtin identity.

The same builtin must never be sent as both a function tool and a provider server
tool in one model request. `AlwaysAsk` requires `HostExecuted`; publication fails
closed when only a provider-side realization exists. Provider-side execution
also carries an explicit maximum-use budget because its internal calls do not
cross the local per-call permission hook. Agent Web execution policy is
realization-independent: when a provider-server projection cannot enforce an
exact domain, content, or location restriction, configuration fails closed and
the publication must select a host realization instead of silently dropping the
policy at the provider boundary.

A host-executed WebFetch selected under an exact domain policy must preserve that
policy across redirects. The configured plugin validates the initial URL with
the shared domain matcher; the selected provider must then apply that same rule
before every redirected request or reject redirects before opening the next
connection. Provider support is explicit and defaults to unsupported. The
direct HTTP adapter rejects redirects while domain policy is active. A managed
Gateway route remains ineligible for that policy until its route contract can
prove target-redirect enforcement, because the open adapter can observe only its
Gateway hop and must not invent a parallel policy inside Cloud.

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
