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

## Phase 12: Workspace ownership and canonical Resource persistence

```text
production config data root -> persisted platform Workspace
scenario-only explicit Workspace -> AWAKEN_SCENARIO_WORKSPACE -> canonical Host
canonical Resource stores -> reopen under the same exact Workspace
```

| Rule | Process type | Workspace input | Resource source | Expected |
|---|---|---|---|---|
| W1 | production | persisted data-root identity | none | exact persisted Workspace |
| W2 | scenario | explicit scenario metadata | canonical Memory + Skill | writes use that exact scope |
| W3 | scenario restart | same explicit metadata | same canonical stores | canonical aggregates remain |
| W4 | production | `AWAKEN_LOCAL_WORKSPACE_ID` | any | ignored; no configuration effect |

The test-only name prevents fixture metadata from becoming a second production
deployment boundary. There is no startup legacy-Resource importer or dual-read
adapter; unreleased pre-baseline databases are recreated before this suite runs.

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

## Phase 15: ephemeral ResourcePlane without inferred durability

```text
scenario DeploymentConfig::ephemeral
  -> canonical in-memory ResourcePlane
  -> production workspace-path adapter
  -> File/Memory ownership + lifecycle
  -> Skill requires its independent durable store
```

| Rule | Resource | Workspace | Durable owner installed | Expected |
|---|---|---|---:|---|
| E1 | File | A then B | no | content-addressed bytes, independent ownership |
| E2 | File | A with live Session edge | no | logical delete; purge after archive |
| E3 | Memory | exact A | no | volatile aggregate lifecycle succeeds |
| E4 | Skill | exact A | no | `409 no durable skill store` |
| E5 | any | path-selected A vs B | no | exact workspace scope; no local default |

The fixture decorates the canonical ephemeral Host with the existing workspace
path adapter. It neither launches production with an implicit missing config nor
adds a volatile Skill store, preserving one owner per Resource kind.

## Phase 16: container provider feature admission

| Rule | Session tier | Compiled provider | Expected |
|---|---|---|---|
| C1 | Docker | none | boot fails before listen |
| C2 | Podman | none | boot fails before listen |
| C3 | Kubernetes | none | boot fails before listen |
| C4 | Podman/Kubernetes | Docker only | exact missing feature error |

The test-only fixed ACP launch selects `SESSION_ENVIRONMENT_TIER`; production
selects the equivalent tier only from typed deployment configuration. Removed
production `AWAKEN_SANDBOX_TIER` input cannot silently select or downgrade a
provider.

## Phase 17: exact Environment projection through pooled containers

```text
official Environment config (network)
  + exact SandboxExecutionPolicy version (root/isolation/limits)
  -> frozen Session baseline
  -> first runtime assignment synchronizes projection
  -> WarmContainerPool delegates inner capability evidence
  -> provider admission -> one Session-owned container
```

| Rule | Network | Sandbox root | Pool capability | Expected |
|---|---|---|---|---|
| P1 | allowlist | host/default | no no-bypass | reject before container creation |
| P2 | none | explicit image | exact inner evidence | Podman execution succeeds |
| P3 | unrestricted | scope/local-dir fallback | exact inner evidence | configured image executes |
| P4 | any | missing private root/tarball | exact inner evidence | fail closed, no default fallback |
| P5 | unrestricted | resource-bearing Session | Docker/Podman | one container survives brain restart and is adopted |
| P6 | allowlist | any | wrapper default Workdir evidence | forbidden capability-loss path |

Runtime assignment synchronizes the baseline even when Resources and MCP are
empty. Provider wrappers are capability-transparent: the warm pool cannot replace
the concrete Docker/Podman evidence with the trait's conservative default. This
keeps Environment networking, sandbox policy, and the physical container on one
causal path.

## Phase 18: opaque Session environment recovery binding

| Rule | Binding damage | Physical environment | Expected |
|---|---|---|---|
| B1 | invalid encoding | live | fail closed |
| B2 | wrong Session id | live | fail closed |
| B3 | wrong provider kind | live | fail closed |
| B4 | missing provider locator | live | fail closed |
| B5 | exact binding | stopped/deleted | fail closed; no replacement |

The scenario explicitly selects Docker through its test-only Session tier. The
Session aggregate remains the sole owner of the opaque binding; retained columns
and a default local/namespace provider cannot participate in recovery.

## Phase 19: typed Postgres persistence axes

| Rule | Typed database fields | Node roots | Credential input | Expected |
|---|---|---|---|---|
| G1 | resource + Session/admin | different | exact binding only | shared File/Memory/Skill/lifecycle truth |
| G2 | catalog/credential/config/admin/Session | replacement | exact references | publication and Session survive restart |
| G3 | same Postgres resource DB | different | raw Repository token | reject before mutation |
| G4 | same Postgres resource DB | different | no raw token | mount-only update succeeds |

Both production processes receive database topology through one generated typed
config. Per-database `AWAKEN_*` variables and node-local fallback stores are not
part of either persistence axis.

For the common single-PostgreSQL management topology, the typed config contains
only `management_database_url_file`. The operator projects one secret file; the
resolver reads it once and supplies catalog, credential, config, admin,
Environment, Session, and Resource stores. Combining that shared file with any direct per-store URL is
rejected as ambiguous rather than creating two topology authorities.

| Shared URL file | Per-store URL | File contents | Expected |
|---|---|---|---|
| present | absent | PostgreSQL URL | all management and Resource stores use the exact projected value |
| present | present | any | reject ambiguous topology |
| present | absent | empty/non-PostgreSQL | reject before store connection |
| absent | present | PostgreSQL URL(s) | existing advanced split-store topology |
| absent | absent | n/a | existing embedded local topology |

## Phase 20: typed remote Worker resource composition

```text
production config ResourcePlane
  -> frozen Session Resource manifest
  -> placement requires resource-capable Worker
  -> explicit embedded Worker deployment + same shared ResourcePlane
  -> exact File/Skill/Memory projection and detach
```

| Rule | Worker ResourcePlane | Manifest | Workspace | Expected |
|---|---:|---|---|---|
| W1 | absent | non-empty | exact | cannot claim |
| W2 | shared Postgres | File/Skill/Memory | exact | claim and realize exact tree |
| W3 | shared Postgres | empty successor | exact | detach prior projection |
| W4 | shared Postgres | valid resource | different | fail closed |

The injected credential resolver remains the sole resolver. The embedded Worker
receives a resolved DeploymentConfig and reuses the canonical shared resource
wiring; removed worker `from_env` configuration is not recreated by the fixture.

## Phase 21: typed production restart durability

```text
one typed deployment data_dir
  -> boot 1 authors model + Session transcript
  -> graceful process stop
  -> boot 2 on a different port, same data_dir
  -> catalog warm-load + original Session continuation
```

| Rule | Deployment root | Process | Port | Expected |
|---|---|---|---|---|
| D1 | exact A | first | P1 | author model and persist Session turn |
| D2 | exact A | replacement | P2 | new Session resolves persisted model |
| D3 | exact A | replacement | P2 | original Session rehydrates transcript |
| D4 | developer home / removed env vars | either | any | cannot affect the scenario |

The durability fixture now reuses the production build, typed deployment, port,
and shutdown harness. The retired management/storage environment variables and
its parallel process fixture are deleted; `data_dir` is the sole persistence
authority across both process lifetimes.

## Phase 22: typed per-component control-store topology

```text
one typed deployment file
  -> default data_dir owns admin + Environment + Session stores
  -> exact catalog/credential/config fields select independent stores
  -> model publication resolves across those store boundaries
```

| Rule | Store field | Configured path | Expected owner |
|---|---|---|---|
| T1 | catalog_db | external A | catalog only at A |
| T2 | credential_db | nested external B | credential only at B |
| T3 | config_db | external C | Agent publication only at C |
| T4 | admin_db / data_subject_db / environment_db / sessions_db / captured_content_db absent | data_dir | default role-owned database files |
| T5 | removed per-component environment variables | any inherited value | no topology effect |
| T6 | `management_database_url_file` only | projected Secret file | all control + Resource stores share one URL |
| T7 | Control has environment_db, sessions_db, or captured_content_db | any | reject before store acquisition; Control uses Coordinator ports only |

`spawnProduction` accepts the same database map already owned by
`deploymentEnv`; per-component tests no longer duplicate binary discovery,
process shutdown, port precedence, or a legacy environment-variable topology.

## Phase 23: environment-independent deployment gate

| Rule | Typed configuration | Conflicting process environment | Expected |
|---|---|---|---|
| C1 | Worker without server | any | reject before config file access |
| C2 | no local pool, no Postgres dispatch | any | reject |
| C3 | path-shaped resource database | any | reject; only embedded root or Postgres |
| C4 | Postgres dispatch, local resource/catalog | any | reject before adapter connection |
| C5 | local Serve at P1/root A | Worker/P2/root B/database/key values | report Serve/P1/root A/SQLite only |
| C6 | business P1 + admin P2 | stale `AWAKEN_HTTP_ADDR` | both typed ports; admin-only drain transition |

The former compatibility test explicitly requiring legacy deployment variables
has been removed. The real binary now proves the opposite invariant: CLI
presentation overrides plus typed config and defaults are the only deployment
causes. Inline seal-key reporting names `config.toml`, never an environment
source, and remains redacted.

## Phase 24: one explicitly test-only scenario deployment boundary

```text
SESSION_DEPLOYMENT_* test metadata
  -> scenario_deployment() exactly once
  -> typed DeploymentConfig
  -> scenario Host / pool / store / Worker composition
```

| Rule | Ingress | Store root | Restart | Expected |
|---|---|---|---|---|
| S1 | durable | exact A | no | background pool commits reply |
| S2 | durable | exact A | yes | history survives and pool resumes |
| S3 | durable | absent | either | fail before listen |
| S4 | direct | any | either | no standing durable pool |
| S5 | retired production-style scenario keys | any | either | no reader exists |

All scenario callers now use the separately named `SESSION_DEPLOYMENT_*` test
metadata. The former `AWAKEN_*` aliases were removed in one migration, not kept
as compatibility fallbacks. Within the scenario host, storage is read by the
single resolver and reused by Skill, Resource, ACP, container, commit, and
dispatch composition; production runtime diagnostics refer only to typed
`DeploymentConfig` fields.

## Phase 25: typed server seal-key custody

| Rule | Mode | Typed key source | Retired environment key | Expected |
|---|---|---|---|---|
| K1 | local | absent | any | create/reuse owner-only local key |
| K2 | server | inline | any | use exact redacted typed value |
| K3 | server | file | any | read exact operator-owned file |
| K4 | server | absent | valid-looking | reject before bind |
| K5 | any | inline and file | any | reject ambiguous custody |

The E2E exercises K4 against the production composition. It no longer asks a
scenario host to infer durable management mode from an old environment variable,
and explicitly proves that the removed key variable cannot bypass typed parsing.

## Phase 26: restart-safe scenario port ownership

| Rule | Preferred port | Availability at reservation | Restart | Expected |
|---|---|---|---|---|
| P1 | free | free | same process test | reserve P1 and reuse across boots |
| P2 | occupied | occupied | same process test | reserve one OS-assigned port and reuse |
| P3 | reserved port | becomes occupied by unrelated process | next boot | fail closed with child exit evidence |

The MCP recovery fixture now consumes the harness's single availability resolver
before its three-boot sequence. It also drops ignored `AWAKEN_MGMT_*` metadata;
the isolated scenario HOME owns management persistence and the separately named
Session deployment root owns runtime persistence.

## Phase 27: production Postgres sandbox-policy authority

```text
typed environment_db = Postgres
  -> production EnvironmentState composition
  -> canonical SandboxExecutionPolicyStore port
  -> Postgres exact-version aggregate
  -> Environment snapshot consumes the pinned revision
```

| Rule | Exact policy | Disabled | Current fence | Expected |
|---|---|---|---|---|
| PG1 | present v1 | false | 1 | bind Environment to immutable v1 |
| PG2 | publish v2 | false | 1 | commit v2; existing binding remains v1 |
| PG3 | publish v3 | false | stale 1 | reject with conflict |
| PG4 | present v1 | true | n/a | reject binding as disabled |

The Postgres E2E now drives the same public policy routes and canonical store
port as the SQLite matrix. This closed a real backend-axis gap without adding a
store-specific API or a second Environment realization path. The changed-line
gate retains only audited pre-bind/composition or corrupt-store branches that no
served TypeScript request can deterministically select; every stale historical
waiver was removed.

## Phase 28: remove the retired management configuration track

| Rule | Typed deployment HOME | Retired management env | Restart | Expected |
|---|---|---|---|---|
| C1 | exact A | absent | A → A | all control/session state survives |
| C2 | exact A | poison value | A → A | poison has no reader and no effect |
| C3 | A → B | absent | restart | B cannot silently consume A |

Three restart E2E fixtures had continued to pass ignored `AWAKEN_MGMT_*` values
while persistence was actually owned by the harness's typed config HOME. They
now call the single `deploymentEnv` fixture explicitly. The unused legacy
seal-key source resolver, its re-export, and its compatibility-only tests were
deleted; current source and test-design documentation now name typed deployment
fields only. This removes a false second configuration path rather than keeping
two inputs synchronized.

## Phase 29: one typed tool-erasure owner

| Rule | Dynamic arguments | Target Args | Tool policy | Expected |
|---|---|---|---|---|
| T1 | `null` | optional/empty DTO | typed `Tool` | normalize to `{}` and execute |
| T2 | exact object | matching DTO | any | deserialize once and execute |
| T3 | missing/unknown field | strict DTO | typed `Tool` | `InvalidArguments` |
| T4 | missing/unknown field | strict DTO | legacy model-visible `RawTool` | one canonical error output |

`awaken-runtime-contract::tool` now owns argument parsing, null normalization,
error classification, and output rendering. Dependency direction keeps the sole
`Erased<T>` implementation in the builtin extension adapter, which delegates all
conversion behavior to that contract mechanism. Admin Assistant and Host
auxiliary tools reuse the same parser; fixed management argument DTOs reject
unknown fields. No second conversion rule remains.
