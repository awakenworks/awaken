# Error Taxonomy

This document makes error ownership explicit. Runtime, config, dispatch,
environment, and public adapters each own different error names. Public error
schemas are projections, not runtime-domain types.

## Error Classes

| Class | Owner | Meaning | Public mapping |
|---|---|---|---|
| `ActivationRejected` | adapter/server | request cannot become `RunActivation` | adapter error |
| `ConfigResolutionError` | Config Domain / `RunResolver` | published data is missing, invalid, or mismatched | adapter maps to setup/config failure |
| `CapabilityMismatch` | runtime resolver/binding validator | required backend/model/tool capability is unavailable | adapter maps to unsupported capability |
| `PermissionDenied` | permission policy | protected operation is denied | adapter maps to authorization or tool-result error |
| `DecisionRequired` | permission/live control | execution is suspended for explicit decision | adapter maps to wait/requires-action shape |
| `ToolExecutionError` | tool/backend adapter | tool ran and returned recoverable failure | usually model-visible tool result |
| `ExternalExecutionIndeterminate` | executor/backend | remote work may or may not have completed | adapter maps to retry/unknown outcome |
| `DispatchError` | Dispatch / Server | durable delivery, lease, or buffering failure | server/protocol error |
| `CommitConflict` | store/commit coordinator | atomic runtime write failed or conflicted | retry or terminal according to store policy |
| `ProjectionError` | adapter/projection sink | committed truth could not be projected | public stream/replay sink error |
| `EnvironmentError` | Runtime/extension (in-process) | filesystem, process, quota, or driver failure while a tool runs | typed tool failure |
| `ProductAdapterError` | product adapter | public protocol compatibility failure | public protocol error |

Runtime code should prefer typed neutral errors. Product adapters decide public
status code, public error type, and public message wording.

## Error Attributes

Every stable error class should expose:

- owner domain;
- retryability;
- whether the run can continue;
- whether the model may see it as a tool result;
- whether it is safe to expose publicly;
- correlation id or run/thread id when applicable;
- source cause for diagnostics;
- redaction status.

## Mapping Rules

1. Runtime errors do not contain public protocol event names or DTO variants.
2. Public adapters map neutral errors into public error schemas.
3. Tool recoverable failures become tool results when the loop can continue.
4. Fatal runtime failures commit a typed terminal reason when persistence is
   enabled.
5. Projection failures do not rewrite committed runtime truth.
6. Indeterminate remote execution is explicit; it is never silently converted to
   success.

## First Vertical Slice

1. Define one neutral error from each owner: activation, capability, permission,
   tool execution, dispatch, commit, projection, and environment.
2. Map them into one public adapter error schema.
3. Prove runtime core does not import public error DTOs.
4. Prove recoverable tool errors are visible to the loop as tool results.
5. Prove terminal errors commit typed terminal facts when persistence is enabled.

## Guardrails

G1, G10, G13, G19, G23, G25, and G26 in [INVARIANTS](../INVARIANTS.md).
