# Tools, Capabilities, And Policy

This doc tells implementers where tool and capability behavior belongs. It follows
the reference design's segmented model instead of creating one broad capability
subsystem.

## Capability Segments

| Segment | Owner | Examples | Development rule |
|---|---|---|---|
| Decision-surface descriptor | Dispatch / Server | agent instructions, model id, tool/skill descriptor, allowed tool ids, content hash | Serialized in `ResolvedSpec` and fingerprinted |
| Execution behavior | Orchestration layer above or runtime catalog | tool impls, skill scripts, MCP servers, remote endpoints | Invoked by id through existing tool/backend ports |
| Operator overlay | Config / Admin / Product | permission allow/deny/ask, skill visibility, HITL policy | Mutable; not part of replayable descriptor truth |
| Secrets / credentials | Product data plane | API keys, OAuth grants, vault refs | Opaque references only across runtime boundaries |
| Session data | Runtime Core | messages, decisions, state facts, verdicts | Replayed from committed runtime facts |

The runtime validates what the model saw and the content hash of execution
material. It does not own execution bytes, vault secrets, or operator workflow.

## Tool Model

Tools are stable runtime objects with serializable descriptors:

- descriptor: id, name, description, schema, policy metadata, content hash;
- implementation: registered runtime tool, host tool, MCP/server tool, or
  remote tool invoked by id;
- policy: explicit permission path, never implied by registration or selection.

Use `Tool` for the preferred typed implementation API. Do not introduce
`TypedTool` as the recommended path: the typed path should be the ordinary path.
Use `RawTool` only for dynamic, schema-erased, or adapter-facing tools such as
MCP/server/client-provided descriptors where compile-time input and output types
are unavailable. `RawTool` remains a low-level escape hatch with explicit schema,
serde, validation, and error mapping.

Runtime core and runtime contracts may define the `Tool` / `RawTool` traits,
descriptor values, call/result envelopes, permission seams, and executor ports.
They must not ship concrete model-callable tool implementations or concrete tool
ids outside tests. Test fixtures may implement fake tools only under test modules
or test crates.

If a tool is visible to a model, that grants perception only. Invocation still
runs through permission and capability checks.

Tool decisions follow the ladder in
[runtime-interface-boundaries.md](runtime-interface-boundaries.md#tool-decision-ladder):
registered catalog, merged tool sources, descriptor visibility, model-visible
descriptors, invocation arguments, gate/permission decision, execution,
`StateCommand`, and commit. Do not collapse visibility, authorization, and
execution location into one capability check.

The runtime-facing execution port stays neutral:

```text
ToolExecutor::execute(call, context) -> ToolOutput
```

Host-hand tools, MCP tools, remote tools, and
client-executed tools implement that port on the execution side. They may use
their own wire protocols, correlation ids, and idempotency rules, but those
details — and where execution happens — do not enter the runtime contract. Runtime sees a descriptor, a validated
call, a gate/permission decision, and a `ToolOutput` whose state changes still
stage through `StateCommand`.

## External Agent And Tool Adapters

External agents are not a new runtime aggregate. Expose them through one of two
adapter shapes:

| Shape | Use when | Runtime sees |
|---|---|---|
| `agent_run` target | the model should explicitly delegate to a configured agent | one tool call with an `agent_id` argument, roster validation, permission gate, and normal tool output |
| `ExecutionBackend` / `RawTool` adapter | an external service or agent runtime executes work behind a selected descriptor | backend/tool request, capability profile, correlation id, and typed or raw result |

The config domain or product adapter owns discovery, endpoint selection,
credentials, remote authorization, and public naming. It publishes only
descriptor data, allowed targets, backend profiles, and opaque refs into runtime
resolution. Runtime validates the selected descriptor and capability evidence,
then invokes by id through the normal tool/backend port.

External agent adapters must provide:

1. a stable descriptor or backend target id;
2. schema and content/fingerprint data when model-visible;
3. capability requirements and advertised backend profile;
4. permission policy keys;
5. correlation/idempotency behavior for retry and resume;
6. typed error mapping, including indeterminate execution.

They must not write runtime state, append messages, publish config, or emit
protocol replay directly. Results return as `ToolOutput`, backend output,
`StateCommand`, `ScheduledAction` result, or neutral resume data and become
durable only through the normal commit path.

## Official Builtin Tools Extension

The runtime core owns no concrete model-callable tool ids. Official tools are
distributed as `awaken-ext-builtin-tools`, not as runtime-core defaults. The
extension package may contain multiple independently enabled toolsets, but they
all enter the runtime through the same plugin and registry seams:

Runtime core may define the generic mechanism used by these tools: descriptor
validation, visibility filtering, permission gates, tool execution ports,
state/effect staging, scheduled-action commit, message commit, and resume
validation. The concrete tool ids, schemas, target choices, recovery policies,
and out-of-process execution adapters live in the extension or adjacent owner. Adding
the extension can change agent behavior; adding the core mechanism alone must
not.

| Toolset | Tool ids | Runtime rule |
|---|---|---|
| `builtin-hand-tools` | `bash`, `read`, `write`, `edit`, `glob`, `grep`, `web_fetch`, `web_search` | descriptors and proxy tools live in the extension; process, filesystem, and network execution stay in the orchestration layer above |
| `builtin-task-tools` | `send_message`, `cancel_task`, `recover_failed_messages` | task tools are ordinary plugin tools over runtime state/effect/commit seams; recovery tools are ops-scoped unless explicitly enabled |
| `builtin-delegation-tools` | `agent_run` | delegation is one tool id with a target argument, not one generated id per target agent |

`agent_run` has a stable descriptor id. Its arguments include at least
`agent_id` and `prompt`; optional arguments such as input metadata, handoff mode,
or parent context must remain data values. Resolution may specialize the
descriptor for one run by adding an enum or metadata list of allowed target
agents, and the descriptor fingerprint must include that target list when it is
model-visible.

Invocation is fail-closed:

1. `agent_run` is hidden when the resolved agent has no delegate or multiagent
   targets.
2. `agent_id` must match the resolved delegate/multiagent roster.
3. Permission rules apply to `agent_run` and may further constrain the
   `agent_id` argument.
4. Execution chooses local or remote backend only after visibility and
   authorization succeed.

Do not reintroduce `agent_run_<agent_id>` as a tool id. If UI or logs need a
friendly label, derive it from `agent_run` plus the `agent_id` argument.

`recover_failed_messages` is a task recovery tool, not a normal business tool:
it inspects and replays the dead-letter list for durable messages that failed
delivery. Default agent profiles should hide it unless the agent has an explicit
operations or recovery role.

## Admin Assistant Tool Boundary

Admin assistant tools are not builtin runtime tools.
`awaken-admin-assistant-tools` may expose admin-only `Tool` implementations, but
those tools are bound through a private admin registry and route/service auth,
not through ordinary agent `plugin_ids` or published
`AgentSpec.allowed_tools`.

Examples include `admin_get_platform_capabilities`, `admin_create_agent_draft`,
and `admin_validate_agent`. They may read capability snapshots or validate draft
config, but they must not carry implicit publish authority. If an admin tool
performs a destructive or publishing action, it needs its own product/security
decision, audit rule, and confirmation policy.

## Tool And Capability Role Catalog

This catalog covers stable roles that decide what the model can see, what a
specific invocation may do, and where execution happens. These decisions stay
separate even when one implementation computes several of them together.

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `ToolDescriptor` | descriptor value | model-visible tool identity, schema, policy metadata, and content hash | resolved catalog, runtime/tool registry | executable code, authorization grant, secret material | model sees unpinned or mismatched tool contract | G3, G8; descriptor fingerprint tests |
| `Tool` | typed tool trait | preferred typed implementation API over declared input/output and runtime context | extension packages, runtime executor adapter | concrete core tool ids, permission bypass, direct durable write | tool behavior leaks into core or typed validation is skipped | G8, G9, G14; typed tool adapter tests |
| `RawTool` | schema-erased tool trait | dynamic call boundary for MCP/server/client tools with explicit schema and serde validation | descriptor, raw call envelope, adapter-owned implementation | recommended typed API, authorization, direct store writes | raw adapter accepts invalid input or bypasses typed policy | G8, G9; raw validation tests |
| `ToolVisibilityPolicy` | visibility policy | descriptor include/exclude and step-time visibility semantics | catalog fields, active plugin scope, step filters | invocation authorization, backend selection | hidden authorization encoded as visibility | G8, G9; visibility policy tests |
| `ToolGateHook` | invocation gate hook | allow, block, suspend, or set result for one tool call | visible descriptor, call arguments, permission policy | descriptor visibility, tool implementation, durable commit | tool invocation bypasses explicit permission path | G9; no-bypass permission tests |
| `ToolPolicyHook` | policy extension hook | compute unconditional or contextual tool policy effects | selected config, runtime context, active plugin scope | model-visible descriptor list, execution transport | preview and runtime apply different permission rules | G8, G9; policy parity tests |
| `ToolExecutor` | runtime execution port | invoke the selected tool through a neutral call/result contract | tool call, resolved descriptor, runtime context | transport protocol details, authorization, direct store writes, execution location | runtime contract depends on one host/transport implementation | G7, G9, G14; tool executor adapter tests |
| `BackendProfile` | capability evidence value | advertised backend features checked before execution | selected backend, activation requirements | provider selection, permission, health authorization | unsupported continuation/tool/decision feature fails late | G7; backend negotiation tests |
| `PermissionPolicy` | authorization policy | explicit allow/deny/ask decision for protected operation | operator overlay, credential refs, runtime context | selection, capability compatibility, visibility | membership or health check becomes authorization | G9; permission type/API tests |
| `CapabilityRequirement` | requirement value | feature demand derived from the resolved run | `ResolvedSpec`, tool descriptors, backend profile | provider search, product policy mutation | runtime silently degrades required capability | G7, G8; fail-closed requirement tests |

When a tool reaches the catalog through a plugin's resolved `Contributions`, its
contribution identity is one value by construction: the registration key, the
`ToolDescriptor` id, and the `CapabilityBound` reference are the same identity,
not three fields kept in sync. A contributed `ToolDescriptor` whose id is outside
the plugin's declared `CapabilityBound` is rejected fail-closed at resolve and at
catalog registration. See
[ADR-0004 D5](../adr/0004-plugin-factory-contributions-and-capability-bound.md)
and guardrail G30.

## Backend And Tool Capability Checks

Use `BackendProfile` and runtime/tool descriptors to check whether a run can be
served:

1. Resolve named references first.
2. Compare required features with advertised features.
3. Fail closed with a typed error on mismatch.
4. Do not search for "some provider that fits" inside runtime code.

Provider/model selection is an application or config-domain concern. Runtime receives
the selected data and validates it.

## Permissions

Selection, compatibility, and health checks never authorize. Their result types
must not contain a grant. Authorization belongs to the permission policy path.

Examples:

- a credential pool may choose a credential candidate, but cannot grant
  `credential.use`;
- a backend may advertise delegated tool execution, but cannot bypass tool policy;
- a successful health probe may clear an availability projection, but cannot mark
  a user authorized.

## First Slice For Skills/MCP/A2A

For a new capability type, implement in this order:

1. Descriptor in `ResolvedSpec` and fingerprint calculation.
2. Runtime validation against the catalog fingerprint.
3. Existing invocation path by id.
4. Permission policy hook.
5. Out-of-process or remote execution only when a concrete driver in the
   orchestration layer above requires it.

This keeps replayability and discovery from competing.

## Non-Goals

- No `ProviderKind` branch tree in runtime code.
- No universal `CapabilityProfile` crate unless several implemented slices need
  the same value object.
- No secret material in serialized specs.
- No authorization hidden in capability compatibility.

## Guardrails

G8 and G9 in [INVARIANTS](../INVARIANTS.md).
