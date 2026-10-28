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

Session admission carries the original neutral Run error through the existing
`RuntimeDiagnostic`, including an owner-defined code that dispatch does not
recognize. Claim disposition continues to come from the realization contract;
preserving a diagnostic never grants permission to replay a tool or changes a
deferral into a failed attempt. Claimed pre-execution settlement commits that
original code. Only an unclassified legacy resolution error uses the generic
`dispatch_resolution_failed` fallback. The Managed protocol remains the owner of
the public error projection.

## Provider stream completion boundary

The provider SDK owns wire parsing; `GenaiExecutor::infer_streaming` maps its
protocol `End` into a neutral response. Runtime alone owns retry, continuation,
tool execution and commit. The adapter's consumption loop returns the `End`
value itself: only that branch can reach response assembly. HTTP EOF before
`End` returns a retryable `Provider` error, including after text/tool deltas;
explicit stream errors and idle/total timeouts retain their existing mapping.
Conversely, `End` completes consumption without requiring transport EOF. Live
deltas remain observations, not completed responses or authorization to execute
partial tools. Existing interrupted-stream checkpoints retain their own
continuation semantics; this rule does not forbid checkpointing live progress.

The cause/effect matrix in provider `tests/stall.rs` exercises both Anthropic
and OpenAI through the pinned SDK over real local HTTP, with text/tool content,
present/absent `End`, and complete/truncated transport bodies. It complements
the existing stall, heartbeat, empty-response and open-connection tests.
These are executable boundary checks, not a formal proof of the SDK/network or
evidence that an unrelated live provider decoding failure has been resolved.

The same provider response guard rejects reserved DSML opening or closing
framing in assistant text when the SDK supplied no typed tool call. Closing
tags after an otherwise ordinary response and incomplete framing are protocol
faults too; the guard never strips them, accepts a JSON prefix, or interprets
them as another executable tool format. Both completed-body and stream-End
paths retain their existing terminal receipt on this retryable
`InvalidResponse`. Runtime's existing bounded retry policy discards the invalid
wire text and retries the original request; it does not continue or checkpoint
that text as an accepted assistant response. Ordinary text and authoritative
SDK-typed tool calls retain their existing handling.

## Returned tool invocations and retained execution domains

ToolBatch owns one validated history fold. Its recorded-domain projection
retains every original entry and resource origin; a distinct outstanding-call
projection may omit an entry only after its exact call is Completed, the batch
is Finalized, and the matching prepared tool-result message is retained.
Clearing a cell, an Indeterminate result, a missing Requested prefix or an
uncompleted earlier retry never supplies that positive observation.

Native attempt recovery uses outstanding invocations for current occupancy and
the complete history for external-process uncertainty. A returned invocation
does not prove that its external process or borrowed Resource stopped. Such
history therefore limits the result to ControlledExecutionClosed unless the
existing provider supplies exact process closure. Model computation and billing
uncertainty remain recorded. This observation neither replays completed tools
nor acknowledges Session cleanup or Resource disposal.

## First Vertical Slice

1. Define one neutral error from each owner: activation, capability, permission,
   tool execution, dispatch, commit, projection, and environment.
2. Map them into one public adapter error schema.
3. Prove runtime core does not import public error DTOs.
4. Prove recoverable tool errors are visible to the loop as tool results.
5. Prove terminal errors commit typed terminal facts when persistence is enabled.

## Guardrails

G1, G10, G13, G19, G23, G25, and G26 in [INVARIANTS](../INVARIANTS.md).
