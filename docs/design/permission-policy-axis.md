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
  -> argument normalization
  -> permission context
  -> PermissionPolicy decision
  -> ToolGateHook / runtime gate
  -> execute, suspend for decision, deny, or set result
  -> staged audit/effect
  -> commit
```

The permission decision must happen for each protected invocation. A cached or
configured policy may provide the answer, but the runtime path still observes an
explicit allow, deny, ask, or substitute-result decision.

## Decision Types

| Decision | Meaning | Runtime behavior |
|---|---|---|
| `allow` | invocation may proceed | execute through the selected tool/backend port |
| `deny` | invocation is not allowed | return typed denied result or terminal error according to policy |
| `ask` | human/operator decision is required | suspend through runtime wait/decision machinery |
| `set_result` | policy supplies a safe result | skip execution and stage the supplied tool result |
| `require_scope` | a narrower credential/resource scope is required | fail or suspend until an explicit grant exists |

No decision grants unrelated authority. Permission for a tool call does not grant
credential write, config publication, provider selection, or admin action.

## Policy Inputs

Allowed policy inputs are:

- resolved descriptor id, schema, content hash, and policy metadata;
- normalized invocation arguments;
- run/thread identity and active agent id;
- selected backend profile and capability requirement;
- operator overlay and explicit grants;
- opaque credential/resource references;
- runtime state facts needed for contextual policy;
- public adapter identity only after it is normalized into an internal caller
  context.

Forbidden inputs are:

- raw public DTOs;
- route-local auth objects in runtime core;
- secret material;
- local absolute paths as authority;
- provider health as an authorization grant.

## Permission Policy Role Catalog

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `PermissionContext` | value object | normalized data for one authorization decision | descriptor, args, caller context, runtime facts | public DTOs, secrets, local paths | policy sees unstable or privileged input | G9, G21; context serialization tests |
| `PermissionPolicy` | policy | allow/deny/ask/set-result decision | operator overlay, explicit grants, runtime context | descriptor visibility, backend selection | selection or health becomes authorization | G9, G21; no-hidden-grant tests |
| `PermissionDecision` | value object | typed decision and reason | policy output | execution transport, public error schema | denied or pending work is ambiguous | G21, G26; decision mapping tests |
| `ToolGateHook` | runtime hook | final invocation gate for a tool call | permission decision, tool call, runtime context | descriptor resolution, direct commit | tool executes without explicit permission path | G9, G21; no-bypass tests |
| `DecisionTicket` | value object | resumable ask/approval correlation (one waiting reason of the shared wait/resume capability) | pending `RunWaitingState`, shared `ResumeValidator` | public session state, config writes | approval resumes the wrong call | G5, G21; resume correlation tests |
| `AuditDraft` | staged fact/effect | reviewable record of protected decision | permission context and decision | durable write outside commit | authorization cannot be explained later | G1, G21; audit commit tests |

## First Vertical Slice

1. Build `PermissionContext` for one protected tool.
2. Return `allow`, `deny`, and `ask` from a `PermissionPolicy`.
3. Gate execution through `ToolGateHook`.
4. Resume an `ask` decision with a correlated ticket.
5. Stage audit output through the normal commit path.
6. Prove visibility, health, and selection cannot grant permission.

## Guardrails

G1, G5, G8, G9, G21, and G26 in [INVARIANTS](../INVARIANTS.md).
