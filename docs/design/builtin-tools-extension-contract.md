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
| `builtin-delegation-tools` | Agent delegation | Runtime extension (local or remote child Run) | one `agent_run` tool with `agent_id` argument |
| `builtin-coordination-tools` | Session-scoped Agent roster and messaging | Runtime extension plus Session coordination | fixed `list_agents` and `send_message`; projected only for a Managed primary Session with a published delegate or Advisor and backed by source Thread state plus a deterministic target Run |

Installing the package does not make every tool visible to every agent. Hand and
native delegation descriptors follow publication selection; the Managed host
projects its fixed coordination pair only into a Managed primary execution clone
with a published delegate or Advisor.

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

### Reading images

The existing `read` tool reads both text and images; it does not require another
image-reading tool. [ReadTool](../../crates/sdk/extensions/awaken-ext-builtin-tools/src/hand.rs)
keeps the same path, permission and confined-root checks for both.

PNG, JPEG, WebP and GIF are identified from file bytes. A valid image produces a
description followed by an image block. Oversized images may be downscaled, with
the ratio reported. Corrupt images, unsupported binary formats and decode/file
limits return explicit errors. The numeric bounds remain owned by
[image preparation](../../crates/sdk/extensions/awaken-ext-builtin-tools/src/hand/image_read.rs).

Whether a tool image reaches the model depends on the selected model route and
request budget. [Provider request shaping](../../crates/sdk/providers/awaken-provider-genai/src/tool_images.rs)
replaces an unsupported or over-budget image with an explanation in the outbound
request; it does not rewrite committed history or imply the model saw the image.
The [file-type decision table](../../crates/sdk/extensions/awaken-ext-builtin-tools/tests/read_image.rs)
and provider tests own the executable cases. Reading an image is not evidence
that a visual task was understood or accepted.

Bash state belongs to one live Hand toolset: working directory, exports and
shell options persist until restart, cancellation, timeout or process exit.
`set -e` therefore also affects later invocations. The one Bash reader drains
stdout and stderr independently; closing one pipe does not prove process exit.
A complete response marker or actual child exit status determines the outcome.
Normal exits preserve their numeric status and bounded diagnostic output; signal
termination remains an error. Cancellation and timeout cover command submission
as well as output reads and reuse the same process-group cleanup. No failed
command is replayed; only a distinct next call starts a fresh shell.

Workspace files belong to the Environment and survive shell/Hand replacement.
A provider's private `/tmp` can be process-local and has no such continuity
contract. Cross-call working files must use the workspace; final files use the
existing output publication directory. This does not expand file-tool roots.

### Web provider realizations

The Web provider catalog is one discovery and validation authority with separate
`WebSearchProvider` and `WebFetchProvider` ports. A provider may implement either
or both ports and a Workspace may bind several accounts for the same provider.
Search and Fetch route plans select one primary target and explicit ordered
fallbacks. They never infer a fallback across credential custody or funding.

For a host-executed paid route, authoring selects only an exact Vault source
revision. The existing plugin publication resolver validates the provider's
descriptor-owned target and usage, then replaces that authoring reference with
one complete, secret-free `CredentialAccess`. Runtime configuration accepts
only the published access; it never reconstructs policy from an id/revision.
Immediately before provider dispatch, the canonical credential materializer
revalidates Workspace, active revision, target, usage, holder, and material
shape, and emits only the narrow outbound HTTP credential. Search and Fetch
providers never receive `CredentialMaterial` or Vault/repository APIs.

Provider wire facts such as Brave's `X-Subscription-Token` live once in the
provider descriptor. They are deliberately absent from the model-visible
authoring schema and application requests. The final HTTP adapter applies the
descriptor-derived wire credential; it does not repeat a provider-specific
header rule. A disabled, rotated, foreign, stale, or mismatched source therefore
fails closed before network I/O. Managed Gateway routes remain a distinct
secretless realization: their short-lived `Authorization` capability authorizes
the Gateway hop and is never confused with or exposed as an upstream provider
credential.

This is a fail-closed snapshot cutover. A pre-cutover host-paid publication that
contains only `credential { id, revision }` must be republished through the same
Config publication command before it can execute; Runtime does not retain a
second legacy policy-construction path.

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

## No Generic Task Command Toolset

The official catalog has no generic builtin Task command family. Managed
cross-Agent messaging uses only the fixed `list_agents`/`send_message`
coordination surface, while native delegation uses `agent_run`. Protocol and MCP
Task cancellation, A2A messaging, generic `RawTool` async-task control, and
operator recovery remain separate typed boundaries; none is duplicated as a
model-callable builtin Task command.

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

## Managed Coordination Tools

A Managed primary Session with a published delegate or Advisor replaces native
delegation descriptors with exactly `list_agents` and `send_message`. A Managed
single-Agent Session receives no coordination tools, and a Managed child receives
neither native delegation nor another coordination surface. `send_message`
commits its request through source Thread state and freezes its input in the
deterministic target Run; it does not install a second mailbox or pending store.

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
