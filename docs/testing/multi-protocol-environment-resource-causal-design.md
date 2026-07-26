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
For the cloud networking sub-axis, the official union is exhaustive:
`unrestricted` permits egress, `limited` with an empty `allowed_hosts` denies
all host egress, and `limited` with hosts is an allow-list. A private `none`
variant or networking on `self_hosted` is rejected rather than normalized.

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

## Phase 6: orthogonal protocol E2E

```text
Environment network U|N × Sandbox default|exact-workdir
× Resource absent|File × Protocol AI-SDK|AG-UI|A2A
    -> Managed Session freezes one baseline
    -> ProtocolHost adopts that exact thread
    -> existing SandboxSpec/provider realizes it
    -> turn commits and projections retain exact Environment/Resource identity
```

The Cartesian decision table contains `2 × 2 × 2 × 3 = 24` rows. Every row must
produce an agent message containing its unique marker, preserve the selected
Environment id, and preserve the Resource cardinality. There is no protocol-specific
configuration input, resolver, or expected-result branch.

## Phase 7: durable composition and restart

```text
typed deployment with storage root
    -> one durable ResourcePlane + Host constructor
    -> stable local Workspace id + Session repository + thread store
    -> restart reconstructs the same owner and exact Session baseline
```

| Storage | Durable ingress | Restart | Session row | Owner scope | Expected |
|---|---:|---:|---:|---:|---|
| absent | no | no | process-local | process-local | normal ephemeral turn |
| present | no | no | durable | persisted | normal durable turn |
| present | yes | no | durable | persisted | durable operations enabled |
| present | any | yes | durable | same persisted id | rehydrate and resume |

The worker-drain axis is independent: `disable_local_pool=false` lets the local
pool claim; `disable_local_pool=true` leaves the same durable queue exclusively
for an authenticated external Worker. Scenario fixtures project that axis into
the same `DeploymentConfig` before Host construction, never as a later override.

Constructing an ephemeral Host and adding storage afterward is forbidden: it mints
a process-specific owner before durability exists and creates a second resource
composition path.
The same rule covers every Resource family, including the delivered Skill store:
durable File, Memory, Skill, lifecycle, Session, and sandbox adapters come from the
one ResourcePlane/deployment constructor and survive or fail together.
Production-config E2E fixtures isolate the standard path itself: a temporary HOME
contains `~/.awaken/config.toml`, whose `data_dir` is the only persisted catalog
root. Inherited user data and removed `AWAKEN_*` compatibility inputs are never
part of an expected result.

Gate composition follows one ordering rule: ordinary authorization is built first,
Skill wiring may decorate that base, and an explicit Host override is applied last.
The resulting decision table is `base only -> base`, `base + Skills -> decorated
base`, and `any default chain + explicit override -> override`; no later plugin may
silently replace the explicit scheduling policy.

## Phase 8: one deployment input in process E2E

```text
OS HOME + standard ~/.awaken/config.toml
    -> typed ResolvedDeployment
    -> one data_dir owns control stores, IAM bootstrap and ResourcePlane
removed AWAKEN_MGMT_* inputs
    -> no deployment effect
```

| Standard config | Identity mode | Workspace catalog | Expected |
|---|---|---|---|
| isolated `data_dir` | no-login | empty | open local control plane, no bootstrap token |
| isolated `data_dir` | self-managed | default | bootstrap token and workspace under that exact root |
| isolated `data_dir` | self-managed | two exact ids | both scopes authorizable; ownership remains isolated |
| isolated `data_dir` | awaken-cloud | remote IAM fields | cached login and remote PDP use the same typed deployment |
| absent | any removed env input | any | removed input cannot select stores, IAM, or credentials |

The shared E2E harness authors this standard config once per isolated deployment.
Individual scenarios supply typed values to that helper; they must not reproduce
TOML serialization or revive environment-variable precedence.

## Phase 9: independent credential-adapter admission

```text
Native adapter capability profile ─┐
                                   ├─> one process capability declaration
ACP route capability profile(s) ───┘      retaining independent alternatives
exact Run backend + holder + source + realization
    -> one complete profile supports all four causes -> claim
    -> facts split across profiles                    -> reject
```

| Rule | Exact route installed | Holder/source/kind in one profile | Credential shape | Expected |
|---|---:|---:|---|---|
| C1 | yes | yes | Gemini bearer/process secret | claim and launch |
| C2 | yes | yes | Codex OAuth/artifact | claim and provision private file |
| C3 | yes | no | synthetic cross-profile tuple | reject admission |
| C4 | yes | yes | Codex bearer only | `credential_driver_required: codex`; no launch |
| C5 | no | any | any | route unavailable; no fallback adapter |

The process dispatch pool and per-Session durable ingress both read the same Host
composition. Flattening profiles into independent holder/source/kind sets is
forbidden because their Cartesian product creates authority no adapter owns.

## Phase 10: production container deployment input

```text
explicit config.toml
  -> bind + data root + ACP profile + Docker image
  -> one ResolvedDeployment
  -> persisted platform Workspace
  -> Environment networking + File/MCP Session resources
  -> one Docker ACP realization
```

| Rule | Typed config | Persisted Workspace | Environment network | File/MCP | Expected |
|---|---:|---:|---|---:|---|
| P1 | exact Docker profile | exact | unrestricted | anonymous + File | launch and materialize |
| P2 | exact Docker profile | exact | unrestricted | authenticated MCP | reject missing no-bypass proof |
| P3 | removed deployment env only | any | any | any | cannot select port/root/image/CLI |
| P4 | inherited operator home | absent | any | any | cannot supply catalog or Workspace |

The public Environment contract owns network policy only. Container image and
realization tier remain deployment/Worker facts; a private Environment `sandbox`
field would duplicate the versioned SandboxExecutionPolicy boundary.

## Phase 11: one scenario Host composition

```text
scenario_deployment()
  -> resource_host_with_deployment()
  -> ResourcePlane + durable queue/store + sandbox provider
  -> scenario-specific decorators (ACP, delegate, tools, skills, gate)
```

| Rule | Storage | Durable | Decorator | Expected |
|---|---:|---:|---|---|
| H1 | absent | no | any | ephemeral canonical Host |
| H2 | present | yes | delegate | parent/child queue and sandbox survive crash |
| H3 | present | yes | ACP/tools/skills | decorator retains the same deployment |
| H4 | present | yes | direct `SharedHost::new` | forbidden duplicate composition |

Scenario behavior is a decorator, never an alternative constructor. The helper
is the only owner of the ephemeral-versus-durable ResourcePlane decision, so a
special protocol cannot silently discard storage, pool, Environment, or Resource
ownership selected by the test deployment.

## Phase 12: Workspace ownership and legacy Resource import

```text
production config data root -> persisted platform Workspace
scenario-only explicit Workspace -> AWAKEN_SCENARIO_WORKSPACE -> canonical Host
legacy resource-api.db -> canonical Memory/Skill import -> receipt -> old DB removable
```

| Rule | Process type | Workspace input | Legacy source | Expected |
|---|---|---|---|---|
| W1 | production | persisted data-root identity | none | exact persisted Workspace |
| W2 | scenario | explicit scenario metadata | Memory + Skill | import into that exact scope |
| W3 | scenario restart | same explicit metadata | source removed | canonical aggregates remain |
| W4 | production | `AWAKEN_LOCAL_WORKSPACE_ID` | any | ignored; no configuration effect |

The test-only name prevents fixture metadata from becoming a second production
deployment boundary. Legacy migration reuses the server's one migration function
and writes only canonical Resource stores; it never installs a dual-read adapter.

## Phase 13: retained Session upgrade under one data root

```text
standard typed data root -> Session repository + Runtime truth
legacy row without aggregate_json -> decode frozen baseline
first root mutation -> write canonical aggregate_json
later legacy-column drift -> ignored
```

| Rule | Canonical aggregate | Legacy columns | Mutation | Expected |
|---|---:|---:|---:|---|
| L1 | absent | valid active | no | decode retained baseline |
| L2 | absent | valid active | yes | write canonical aggregate once |
| L3 | present | poisoned | any | canonical aggregate wins |
| L4 | absent | terminal | no | no resurrection |

Management and Runtime persistence use the same resolved data root. Removed
`AWAKEN_MGMT_*` and runtime storage variables cannot recreate split Session
authority in either production or process E2E.

## Phase 14: one production process fixture and canonical resource recovery

```text
typed config.toml + CLI port
  -> one production composition
  -> canonical Session aggregate_json
  -> Resource activation/reclamation recovery
  -> exact durable terminal outcome
```

| Rule | Deployment source | Session state source | Fault | Expected |
|---|---|---|---|---|
| R1 | typed config | canonical aggregate | process death after prepare | same generation becomes active |
| R2 | typed config | retained legacy columns | first recovery | one-way upgrade, then canonical aggregate |
| R3 | typed config | canonical aggregate | corrupt catalog revision | fail closed; repair resumes exact generation |
| R4 | typed config | canonical aggregate | reclaim fence/store fault | durable retry without duplicate purge |
| R5 | removed `AWAKEN_*` deployment vars | either | any | no effect on port, root, seal, or stores |

All production resource scenarios use `spawnProduction`; binary discovery,
typed deployment construction, process lifecycle, and readiness are no longer
copied per protocol test. Fault injection edits `aggregate_json` as one value.
Only the explicit retained-row case clears it and writes legacy columns, so the
test cannot accidentally create a second live Session state authority.
