# Resources, Memory, Files, And Skills

This doc defines how product resources are made available to runtime execution
without letting filesystem paths, secrets, or product DTOs enter the runtime
core. Resources are a boundary contract, not a second execution model.

## Owning Context

| Item | Owner |
|---|---|
| Runtime state facts, messages, verdicts | Runtime Core |
| Resource declarations and policy overlays | Config / Product |
| File, memory, vault, skill, and external data stores | Product data plane |
| Credential mechanics and durable resource hosting | Product data plane (out of scope here) |
| Tool invocation and backend execution | Runtime ports and execution adapters |

## Resource Boundary

Runtime receives logical resource requirements as serializable data inside the
resolved run input. It may see ids, content hashes, descriptors, and opaque
references. It must not see absolute host paths, vault secrets, live store
handles, product scope objects, or product DTOs.

The environment realizes resources locally:

```text
ResolvedSpec resource refs -> environment realization -> runtime/backend/tool ports
```

The same resource reference may realize to different local paths on different
hosts. That is expected. Runtime behavior must depend on the logical reference
and validated descriptor, not the realized path.

## Memory And Files

Memory and files are product or environment resources exposed by logical
reference:

- **Memory** is mutable product state. Runtime may read or write through an
  approved tool or resource port, and the product data plane owns consistency,
  quotas, indexing, and sharing rules.
- **Files** are content-addressed or product-addressed objects. Runtime sees the
  declared reference and descriptor; environment code owns local realization,
  read/write mode, mounts, and cleanup.

No runtime API should accept an absolute path as the source of authority. If a
tool needs a local path, that path is produced in-process when the tool runs,
from the resolved logical ref, and is not carried as runtime authority.

## Skills And MCP

Skills and MCP servers are capability material, not runtime policy shortcuts:

1. Product or config-domain code owns public skill/version APIs, bundles, visibility,
   and operator policy.
2. Resolved run input carries descriptors, allowed ids, content hashes, and
   opaque refs.
3. Runtime validates the descriptor/fingerprint against its catalog.
4. Execution happens by id through the existing tool/backend path.
5. Environment code owns any mounts, subprocesses, sidecars, or injected
   credentials.

An unavailable required skill, MCP server, mount, or tool implementation is a
typed pre-execution failure. It is not silently converted into instructions-only
behavior.

## Portable Sessions And Recovery

Portable execution state is split deliberately:

- runtime facts, messages, tool decisions, and verdicts are durable runtime
  state and replay through the commit path;
- product resources, memory stores, files, skills, and vaults are re-bound by
  logical reference;
- environment-local paths, process ids, and mounts are never
  replayed as durable runtime truth.

Warm reuse is an optimization. Cold replay from committed runtime facts must
remain the correctness baseline. State-portable backends can add import/export
support only through explicit backend capability checks.

## External Work

External work is tool or backend execution offload. It does not create a parallel
session dispatcher.

Allowed shapes:

- direct in-process execution through runtime ports;
- durable server ingress that resumes committed runtime work;
- remote or out-of-process backend adapters that execute selected work
  and return typed results.

One run must have one coherent execution shape at a time. Mixing unrelated
dispatch paths inside the same run creates replay and authorization ambiguity.

## First Vertical Slice

For a new resource-backed capability, implement the smallest path that proves the
boundary:

1. Add descriptor/ref data to the resolved run input.
2. Include it in the catalog fingerprint or descriptor hash.
3. Validate it in runtime before execution.
4. Realize it in-process when the tool runs, from the resolved ref.
5. Invoke it through the existing in-process tool port.
6. Add a replay or recovery test showing the run does not depend on local paths.

## Non-Goals

- No product resource store inside runtime core.
- No absolute path, secret value, product scope, or live registry across the
  runtime boundary.
- No separate session dispatcher for external work.
- No authorization implied by resource visibility, skill selection, health, or
  successful mount realization.

## Guardrails

G3, G4, G8, G9, and G13 in [INVARIANTS](../INVARIANTS.md).
