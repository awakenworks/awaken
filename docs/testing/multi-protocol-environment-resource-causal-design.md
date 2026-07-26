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

`E1` and `E2` are mutually exclusive. `E3` and `E4` are forbidden regardless
of placement. The Console must obey the same contract as an official SDK.

### Effects

| ID | Effect |
|---|---|
| R1 | Environment creation succeeds and round-trips the official union. |
| R2 | Creation fails with a stable bad-request response. |
| R3 | The Console emits no private Environment fields. |

### Graph

```text
(E1 xor E2) and not E3 and not E4 -> R1
E3 or E4                         -> R2
E5                               -> R3
```

### Decision table

| Cause/effect | P1 cloud | P2 self-hosted | P3 runtime | P4 sandbox | P5 Console |
|---|---:|---:|---:|---:|---:|
| E1 | 1 | 0 | - | - | 1 |
| E2 | 0 | 1 | - | - | 0 |
| E3 | 0 | 0 | 1 | 0 | 0 |
| E4 | 0 | 0 | 0 | 1 | 0 |
| E5 | 0 | 0 | 0 | 0 | 1 |
| R1 | 1 | 1 | 0 | 0 | 1 |
| R2 | 0 | 0 | 1 | 1 | 0 |
| R3 | 0 | 0 | 0 | 0 | 1 |

Later phases extend this document with Agent defaults, exact Resource resolution,
SandboxExecutionPolicy binding, protocol orthogonality, and restart behavior.
