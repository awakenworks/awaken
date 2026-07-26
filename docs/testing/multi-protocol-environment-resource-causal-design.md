# Multi-protocol Environment and Resource cause-effect design

This document is the executable test design for making Agent-bound Environment
and Resource defaults available through every protocol front door. Each phase
must update this graph and its decision table before production code changes.

## Phase 1: one Environment contract

### Causes

| ID | Cause |
|---|---|
| E1 | The request uses the official `cloud` config fields only. |
| E2 | The request uses the official `self_hosted` config fields only. |
| E3 | The request embeds Awaken `runtime` in Environment config. |
| E4 | The request embeds Awaken `sandbox` in Environment config. |
| E5 | The Console creates the request. |
| E6 | The management assistant creates the request. |

`E1` and `E2` are mutually exclusive. `E3` and `E4` are forbidden regardless
of placement. The Console must obey the same contract as an official SDK.

### Effects

| ID | Effect |
|---|---|
| R1 | Environment creation succeeds and round-trips the official union. |
| R2 | Creation fails with a stable bad-request response. |
| R3 | The Console emits no private Environment fields. |
| R4 | Assistant authoring passes through the same canonical union guard. |

### Graph

```text
(E1 xor E2) and not E3 and not E4 -> R1
E3 or E4                         -> R2
E5                               -> R3
E6                               -> R4 and (R1 or R2)
```

### Decision table

| Cause/effect | P1 cloud | P2 self-hosted | P3 runtime | P4 sandbox | P5 Console | P6 assistant valid | P7 assistant private |
|---|---:|---:|---:|---:|---:|---:|---:|
| E1 | 1 | 0 | - | - | 1 | 1 | 0 |
| E2 | 0 | 1 | - | - | 0 | 0 | 1 |
| E3 | 0 | 0 | 1 | 0 | 0 | 0 | 1 |
| E4 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| E5 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| E6 | 0 | 0 | 0 | 0 | 0 | 1 | 1 |
| R1 | 1 | 1 | 0 | 0 | 1 | 1 | 0 |
| R2 | 0 | 0 | 1 | 1 | 0 | 0 | 1 |
| R3 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| R4 | 0 | 0 | 0 | 0 | 0 | 1 | 1 |

Later phases extend this document with Agent defaults, exact Resource resolution,
SandboxExecutionPolicy binding, protocol orthogonality, and restart behavior.

## Phase 2: one defaults compiler

### Graph

```text
Agent defaults + no Session attachment       -> inherit defaults
Agent defaults + explicit replaces           -> replace named default
Agent defaults + implicit id/path collision  -> reject
Selected Environment snapshot                -> preserve exact revision/fingerprint
```

### Decision table

| Cause/effect | D1 inherit | D2 replace | D3 collision |
|---|---:|---:|---:|
| Agent default exists | 1 | 1 | 1 |
| Session attachment exists | 0 | 1 | 1 |
| Explicit `replaces` | - | 1 | 0 |
| Compile succeeds | 1 | 1 | 0 |
| Exact Environment preserved | 1 | 1 | 0 |
| Collision error | 0 | 0 | 1 |

## Phase 3: exact Agent default bundle

```text
Agent default Environment id + matching revision -> compile that snapshot
Agent default Environment id + stale revision    -> fail closed
Session explicit Environment override            -> resolve the explicit selection
Resource-only update                             -> preserve Environment binding
Current default-bundle revision != publication   -> reject projection
```

| Cause/effect | A1 exact | A2 stale | A3 explicit override | A4 resource update |
|---|---:|---:|---:|---:|
| Agent Environment binding | 1 | 1 | 1 | 1 |
| Registry revision matches | 1 | 0 | - | 1 |
| Explicit Session selection | 0 | 0 | 1 | 0 |
| Session can compile | 1 | 0 | 1 | 1 |
| Current revision substituted | 0 | 0 | 0 | 0 |
| Binding survives Resource update | - | - | - | 1 |

## Phase 4: protocol convergence

```text
Managed create                         -> create/freeze Session baseline
AI SDK | AG-UI | A2A first fresh turn -> same SessionDefaultsPreparer
Existing thread                       -> idempotent reuse
Preparation failure                   -> no Host run
```

| Cause/effect | M1 Managed | M2 AI SDK | M3 AG-UI | M4 A2A | M5 repeat | M6 invalid default |
|---|---:|---:|---:|---:|---:|---:|
| Managed create | 1 | 0 | 0 | 0 | 0 | 0 |
| Protocol fresh turn | 0 | 1 | 1 | 1 | 1 | 1 |
| Existing Session | 0 | 0 | 0 | 0 | 1 | 0 |
| Defaults valid | 1 | 1 | 1 | 1 | 1 | 0 |
| Same baseline path | 1 | 1 | 1 | 1 | 1 | 1 |
| Runtime invoked | 0 | 1 | 1 | 1 | 1 | 0 |

## Phase 5: versioned SandboxExecutionPolicy

```text
typed policy v1 + matching current fence -> immutable v1
publish v2 + expected current v1        -> immutable v2; v1 remains addressable
Environment exact ref v1                -> snapshot freezes v1 after v2 exists
missing/disabled ref                     -> binding or snapshot fails closed
policy network field                     -> reject (Environment is sole network owner)
snapshot                                 -> existing SandboxOverride -> SandboxSpec -> provider
```

| Cause/effect | S1 create | S2 publish | S3 bind old | S4 stale publish | S5 missing | S6 network overlap |
|---|---:|---:|---:|---:|---:|---:|
| Typed policy | 1 | 1 | 1 | 1 | - | 1 |
| Expected current matches | - | 1 | - | 0 | - | - |
| Exact target exists | - | - | 1 | - | 0 | - |
| Network field absent | 1 | 1 | 1 | 1 | - | 0 |
| Commit succeeds | 1 | 1 | 1 | 0 | 0 | 0 |
| Current version substituted | 0 | 0 | 0 | 0 | 0 | 0 |
| Existing SandboxSpec/provider path | 1 | 1 | 1 | - | - | - |
