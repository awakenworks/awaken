# Permission Policy Axis

Permission is its own runtime-facing axis. It decides whether a specific
operation may happen after the model has selected an action and before execution
or resume consumes authority.

Visibility, compatibility, health, and selection are not
authorization. They can narrow what is possible, but only the permission path can
allow a protected operation.

## Permission Flow

```text
resolved descriptor
  -> model-visible tool list
  -> model tool call
  -> normalized ToolCall
  -> ToolPermissionPolicy verdict
  -> PermissionGate / ToolGateHook
  -> GateOutcome
  -> execute, block, require confirmation, set result, or schedule
  -> staged audit/effect
  -> commit
```

The permission decision must happen for each protected invocation. A cached or
configured policy may provide the answer, but the runtime path still observes an
explicit `ToolPermissionVerdict`. Only `Allow` reaches execution;
`RequireConfirmation` commits the ordinary closed wait/resume authority. The
gate-only `SetResult` and `Schedule` outcomes do not enlarge the policy verdict
vocabulary.

## Decision Types

| Policy verdict | Gate outcome | Runtime behavior |
|---|---|---|
| `ToolPermissionVerdict::Allow` | `GateOutcome::Allow` | execute through the selected tool/backend port |
| `ToolPermissionVerdict::Deny { reason }` | `GateOutcome::Block { reason }` | skip execution and return the typed blocked result |
| `ToolPermissionVerdict::RequireConfirmation { correlation_id }` | `GateOutcome::RequireConfirmation { correlation_id }` | commit a closed permission wait and require a typed resume decision |

`GateOutcome::SetResult` and `GateOutcome::Schedule` are final-gate outcomes,
not additional `ToolPermissionVerdict` variants. A narrower credential/resource
scope therefore maps to `Deny` or `RequireConfirmation`; it does not create a
parallel permission control path.

No decision grants unrelated authority. Permission for a tool call does not grant
credential write, config publication, provider selection, or admin action.

## Policy Inputs

The per-evaluation `ToolPermissionPolicy` input is exactly the normalized
`ToolCall` (`tool_id`, `call_id`, and arguments). The concrete policy object may
already own its configured rules and mode. It receives no Run, Thread, backend,
credential, resource, or adapter identity. A final `ToolGateHook` separately
receives the same `ToolCall` plus the Run's read-only `Store`; that hook may
further restrict an allowed call but cannot widen the permission verdict.

Forbidden inputs are:

- raw public DTOs;
- route-local auth objects in runtime core;
- secret material;
- local absolute paths as authority;
- provider health as an authorization grant.

## Permission Policy Role Catalog

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `ToolCall` | value object | normalized data for one authorization decision | tool id, call id, arguments | public DTOs, secrets, local paths | policy sees unstable or privileged input | G9, G21; tool normalization tests |
| `ToolPermissionPolicy` | policy | one `ToolPermissionVerdict` for a normalized call | configured rules and mode | descriptor visibility, backend selection, Run state, execution | selection or health becomes authorization | G9, G21; no-hidden-grant tests |
| `ToolPermissionVerdict` | closed value | `Allow`, `Deny`, or `RequireConfirmation` with its payload | policy output | gate-only outcomes, resume decisions, public error schema | denied or pending work is ambiguous | G21, G26; verdict-to-gate tests |
| `ToolGateHook` | runtime hook | final `GateOutcome` for a tool call | normalized call and read-only Run `Store` | descriptor resolution, direct commit | tool executes without the gate chain | G9, G21; no-bypass tests |
| `ResumeTicket` | closed value | exact confirmation correlation and `AwaitTarget::ToolCall { reason: ToolAwaitReason::Permission, call_id, tool }` | shared `ResumeValidator` | a second ticket type, public session state, config writes | a decision resumes the wrong call | G5, G21; resume correlation tests |
| `PermissionDecision` | closed resume value | the later operator `Allow` or `Deny` answer | `ResumeResult::Permission` | policy evaluation, tool results, free-form input | an untyped input approves a tool | G5, G21; result-kind validation tests |
| `AuditDraft` | staged fact/effect | reviewable record of the gate outcome | normalized call and gate decision | durable write outside commit | authorization cannot be explained later | G1, G21; audit commit tests |

## First Vertical Slice

1. Normalize one protected invocation as a `ToolCall`.
2. Return `Allow`, `Deny`, or `RequireConfirmation` from a
   `ToolPermissionPolicy`.
3. Project the verdict once to `GateOutcome` and gate execution through
   `ToolGateHook`.
4. Commit confirmation as a closed `ResumeTicket`, then accept only
   `ResumeResult::Permission(PermissionDecision::Allow | PermissionDecision::Deny)`.
5. Stage audit output through the normal commit path.
6. Prove visibility, health, and selection cannot grant permission.

## Guardrails

G1, G5, G8, G9, G21, and G26 in [INVARIANTS](../INVARIANTS.md).
