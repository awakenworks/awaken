# ADR-0069: ACP Capability and Configuration Lifecycle

- Status: Proposed
- Date: 2026-07-28
- Implemented foundation (2026-07-28): the repository has one reusable
  local-ACP application service, one adapter catalog, atomic expiring Worker
  observations, prompt-free capability negotiation, generated Agent option
  contracts, publication/placement/launch fingerprints, provisioning-derived
  Session Environment selection, live readiness, and shared CLI/Flow
  composition, descendant process-group reaping, event-triggered all-scope
  publication reconciliation, typed capture authoring, request-grained subject
  attribution, subject-scoped erasable ACP session homes, and the product-level
  Art.17 fan-out E2E. The Agent lifecycle now has explicit
  Published/Disabled/Archived admission and fingerprint-retention behavior.
  PostgreSQL pool sizing is also resolved once as a non-zero
  typed deployment value and injected into both runtime stores. A real
  persisted-login Claude session remains an external release gate. The typed
  runtime-configuration migration is complete; only admitted OS/trusted-host
  metadata and explicit test gates remain ambient. “Proposed” therefore means
  the complete decision is not yet accepted; it does not mean the listed
  foundation is hypothetical.
- Amends:
  [ADR-0057](0057-unified-agent-configuration.md), especially its
  backend-owned trusted-host amendment
- Builds on:
  [ADR-0031](0031-config-store.md),
  [ADR-0041](0041-sandbox-execution-environment-provider.md),
  [ADR-0054](0054-safe-loop-boundary-shared-seam-and-pause-as-durable-await.md),
  [ADR-0062](0062-published-inference-access-and-runtime-credential-injection.md),
  [ADR-0065](0065-recoverable-embeddable-remote-worker.md), and
  [ADR-0067](0067-credential-custody-model-exposure-and-secret-delivery.md)

## Context

ADR-0057 defines one Agent configuration, one immutable publication path, and
two mutually exclusive model-provisioning variants: Awaken-managed `Provider`
and CLI-managed `BackendOwned`. Its trusted-host amendment establishes local ACP
login discovery, WorkerLocal liveness, placement and pre-launch revalidation.
At the time this decision was written, the remaining lifecycle was incomplete:

- installation discovery, login liveness and ACP protocol capability
  negotiation are treated as one concern even though they have different
  frequency, evidence and failure behavior;
- `session/new` and `session/load` advertise session modes and config options,
  but only exact-model delivery currently uses one config-option slot;
- adapter-specific launch/config differences, live capabilities and user
  selections have no complete ownership map;
- the CLI owns local-ACP composition, while Flow owns overlapping ACP inventory;
- local ACP preparation globally selects the Workdir tier instead of choosing a
  Session Environment from immutable provisioning;
- readiness is a startup projection rather than a live Worker observation;
- a generic credential material interface also carries WorkerLocal liveness,
  even though backend-owned login has no material to resolve;
- handwritten frontend model types cannot represent the existing Rust
  `BackendDefault` selection.

Each ACP adapter may expose different modes and configuration:

```text
Codex: model, reasoning effort, approval preference, sandbox preference, ...
Claude: adapter-specific session modes/options
Other ACPs: different native ids, types, values and delivery mechanisms
```

Awaken needs a uniform lifecycle and UI without guessing that similarly named
options have identical semantics, copying the adapter's capability inventory
into Agent configuration, or adding adapter-id branches to the Runtime or
executor.

## Decision

The lifecycle has one rule:

> Normalize the structure and lifecycle of ACP capabilities, while preserving
> every adapter's native option ids and values. Semantic equivalence exists only
> when a versioned adapter descriptor declares it.

No second ACP inventory, configuration repository, Worker kind, executor,
permission engine or Session Environment abstraction is introduced.

### End-to-end target and current focus

Every supported ACP follows one lifecycle:

```text
catalog declaration
  -> Worker installation/login discovery
  -> Worker ACP capability negotiation
  -> effective capability observation
  -> Agent intent authoring
  -> immutable publication
  -> placement and claim
  -> launch-time revalidation
  -> resolved launch
  -> protocol execution and terminal outcome
  -> credential-excluding, subject-scoped session harvest
  -> retention expiry or Art.17 erasure
```

The immediate focus is the boundary between Worker preparation and execution.
Discovery and negotiation are Worker application/provisioning concerns. Runtime
Host uses their immutable result to choose an environment and construct a
launch. `awaken-run-executor-acp` and `awaken-protocol-acp` execute that launch
and validate handshake evidence, but do not scan PATH/HOME, install wrappers,
probe login, maintain an adapter inventory, persist observations or choose
configuration.

“Capability” has two related but distinct meanings:

- a **discovered capability** is a Worker-owned, expiring observation used for
  authoring, publication, placement and readiness;
- a **session-advertised capability** is launch-time protocol evidence used to
  fail closed if the claimed Worker no longer matches the publication.

The latter verifies the former; it is not a second discovery repository.

### D1 — One static descriptor plus one live observation

The existing `AcpCli` catalog evolves into the one versioned adapter descriptor:

```text
AcpAdapterDescriptor
  id
  supported_version_range
  acquisition
  launch
  discovery
  model_delivery
  mcp_delivery
  session_persistence
  config_home
  option_annotations[native_option_id]
```

The descriptor owns protocol-external facts that ACP negotiation cannot reveal:
wrapper acquisition, executable argv, login-status command, config-home layout,
session storage and legacy flag/config-file delivery.

Live ACP negotiation owns the installed adapter's current protocol facts:
advertised modes, config options, value schemas, choices, reported defaults and
protocol capabilities. Static data may annotate live options but may not
override their type, allowed values or default.

Worker application logic, Runtime and execution paths never branch on adapter
id. Adding an adapter adds descriptor data or one leaf delivery adapter.

The descriptor contract is owned outside the executor. During migration the
existing `AcpCli` value remains authoritative, but its declaration and probe
types move to an ACP catalog/contract module. The actual host-process probe
moves from `awaken-run-executor-acp::host_discovery` to
`awaken-acp-application`. This is a move, not a copied compatibility facade.
After callers migrate, the executor has no discovery exports.

### D2 — Three observations, not one discovery operation

1. **Installation discovery** observes executable presence, version
   compatibility and exact wrapper availability. It runs at startup, explicit
   refresh and adapter-version change.
2. **Credential liveness** invokes only the CLI's documented non-interactive
   status command using the same PATH/HOME allowlist as trusted-host launch. It
   periodically returns a secret-free `CredentialObservation` and reruns
   immediately before launch. It never opens the backing login file.
3. **Capability negotiation** is performed by the Worker application service.
   It starts a bounded short-lived ACP process and runs
   `initialize` plus `session/new` to observe modes, config options and protocol
   capabilities. It sends no user prompt, Workspace document body, live resource
   handle or provider credential material.

The capability-probe process owns no user Session continuity, commits no Run
facts, has a fixed timeout and is reaped as a unit.

The Worker application reuses the protocol crate's canonical handshake state
machine. It may not implement a second JSON-RPC parser or duplicate
`initialize`/`session/new` sequencing. The protocol crate exposes a narrow
channel-in/channel-out negotiation operation; process creation, timeout,
observation revision and persistence remain outside it.

The dependency is inverted through `awaken-acp-contract`: the contract owns the
neutral descriptors and `AcpCapabilityHandshake` port,
`awaken-protocol-acp` implements the port, and a composition root injects that
implementation into `awaken-acp-application`. The Worker application never
depends on a `protocol-*` adapter.

### D3 — Structurally uniform, semantically native options

Live options normalize to:

```text
AcpOptionDescriptor
  native_id
  label
  description?
  value_schema
  current_value?
  reported_default?
  choices[]
  delivery
  semantic_facet?
  safety_class
  provenance
```

`value_schema` supports Boolean, String, Integer with optional bounds, Number,
Enum and StringList. Opaque objects require a schema-constrained advanced
editor; arbitrary JSON is not a normal authoring surface.

The generic delivery vocabulary is:

```text
AcpOptionDelivery
  SessionMode
  SessionConfigOption { config_id }
  LaunchFlag { flag }
  ConfigOverride { flag, key }
  ConfigFile { relative_path, key_path }
  Unsupported
```

`semantic_facet` is optional and descriptor-declared. Standard facets may
include Model, ReasoningEffort, InteractionMode, ApprovalPreference and
BackendSandboxPreference. A `model_reasoning_effort`, `thinking_level` and token
budget remain different native options unless a descriptor explicitly maps
them. Native id and native value remain the execution authority.

All ACPs can therefore be uniformly recognized without pretending they have
uniform configuration:

| Uniform across adapters | Preserved as adapter-native |
|---|---|
| id, label, description and provenance | native option id |
| value schema and validation result | native value vocabulary |
| current/default/choice structure | delivery interface |
| safety classification | mode and option semantics |
| verified/stale/incompatible lifecycle | dependency/order constraints |

An adapter that advertises an unknown scalar option is immediately renderable
through the generic contract. A known option may additionally receive a
versioned semantic annotation. An opaque or security-sensitive option remains
advanced, unsupported or forbidden until its schema and authority effect are
proved. This is progressive recognition, not a lowest-common-denominator
configuration model.

### D4 — One effective Worker capability profile

The local-ACP application merges live negotiation with catalog annotations:

| Rule | Live declaration | Catalog annotation | Effective status |
|---|---|---|---|
| C1 | present | compatible | `Verified`; live schema and values win |
| C2 | present | absent | `Verified`; generic native option |
| C3 | absent | present | `DeclaredUnverified`; draft-only |
| C4 | present | conflicting | `Incompatible`; publish/run denied |
| C5 | absent | absent | unsupported and not displayed |

The resulting `EffectiveAcpCapabilityProfile` carries adapter id/version,
availability, modes, options, evidence, observation time and a deterministic
fingerprint. It is a Worker-owned read projection, not an authoring aggregate or
second adapter catalog.

### D5 — Agent configuration persists intent only

The single ADR-0057 Agent aggregate gains one typed ACP selection:

```text
AcpSessionConfiguration
  mode?: native session-mode id
  options: map<native option id, native string value>
```

It is carried by the existing `ModelSelection::Target`, `BackendDefault`, or
`BackendExact` variant; there is no parallel ACP selection record. The Managed
Agents ACL projects the optional `model.x_awaken.acp` object into this value.
Omission retains the official Managed model shape.

- `BackendDefault` sends no exact-model override.
- Exact ACP model selection uses the existing exact/pinned selection projected
  to `BackendModelSelection::Exact` and requires proven delivery.
- `Target` combines the same configuration with a Provider-backed ACP route;
  Provider and BackendOwned publication both freeze one `AcpExecutionProfile`.
- An omitted mode or option means use the backend default; Awaken sends no
  override and never copies a reported default into the Agent.
- The Agent stores no discovered option schema, choice list or availability.

A draft may retain a temporarily unavailable backend or
`DeclaredUnverified` option. Publication requires a fresh `Verified` profile
and rejects unknown ids, wrong types, unsupported values, unsupported exact
model delivery and incompatible selections.

Rust wire types generate JSON Schema/OpenAPI/TypeScript. The UI renders the
descriptor's value schema and choices; it has no Codex/Claude branch and raw
JSON passes the same server validation.

### D6 — Publication freezes one executable demand

Publication resolves the Agent intent against one fresh effective profile and
freezes:

```text
backend_ref
Provider | BackendOwned provisioning
BackendDefault | Exact model policy
exact WorkerLocal CredentialRef/revision for BackendOwned
selected native mode and option values
ACP adapter version and capability fingerprint in one `AcpExecutionProfile`
SessionEnvironmentRequirement
```

`SessionEnvironmentRequirement` is derived from provisioning:

```text
Provider     -> Isolated
BackendOwned -> TrustedHostIdentity
```

It is not a separately authored boolean. Placement intersects backend
capability, exact live credential revision, compatible capability fingerprint
and environment capability. No match is a visible unavailable/gated outcome;
placement never substitutes another backend, Provider route, model or option.

### D7 — One Worker selects an environment per Session

Local ACP discovery does not depend on a global `SandboxTier::Local`. A
local-mode Worker may advertise both isolated and trusted-host environments:

| Provisioning | Environment | Credential handling |
|---|---|---|
| Provider | configured Namespace or Container | exact managed materialization into isolated HOME |
| BackendOwned | Workdir | host PATH/HOME allowlist; no materialization |

This is one Worker, claimer, Session lifecycle and executor. It does not create a
local-ACP Worker type, second pool or second channel source. Failure to realize
the required environment is terminal and never degrades to another tier.

### D8 — Launch revalidates all mutable evidence

Immediately before process launch the claimed Worker:

1. verifies exact WorkerLocal credential id/revision;
2. reruns credential liveness;
3. verifies executable and resolved wrapper evidence;
4. realizes the required Session Environment;
5. establishes ACP and reads live modes/config options;
6. checks frozen native selections against those descriptors;
7. constructs one `ResolvedAcpLaunch`.

`ResolvedAcpLaunch` contains argv, explicit environment, cwd, model delivery,
optional mode, typed config-option selections and exact MCP projection. The
executor consumes it and performs no discovery, repository lookup, backend
selection or fallback.

The capability fingerprint covers adapter identity/version and protocol-level
transport features. It deliberately excludes the mode and config-option
catalogues because an ACP may derive those from the provisioned provider/model
route. Publication still freezes every requested native id/value, and launch
fails closed unless the realized Session advertises each requested selection.

The wire sequence is:

```text
initialize
  -> session/new | session/load
  -> validate advertised modes/config options
  -> session/set_mode                         [explicit selection only]
  -> session/set_config_option(config_i)*    [explicit values only]
  -> session/prompt
  -> session/request_permission*
  -> neutral events/tool results/terminal
  -> commit and credential-excluding session harvest
  -> process-tree reap
```

Multiple options replace the current single model-only slot. Ordering is
descriptor-declared when dependencies exist and otherwise stable by native id.
A missing or rejected option is `ConfigurationIncompatible`, not permission to
omit it or use the default.

### D9 — ACP preferences never expand Awaken authority

Options named `approval_policy`, `sandbox_mode`, `full_access` or similar are
backend behavior preferences, not Awaken authorization or environment policy.

```text
effective authority =
  Awaken authorization
  INTERSECT ToolPermissionPolicy
  INTERSECT realized Session Environment capability
  INTERSECT ACP request
```

The ACP may request less authority but cannot request or configure more.
`approval_policy=never` cannot bypass Awaken HITL;
`sandbox_mode=danger-full-access` cannot escape Namespace/Container or convert
Provider provisioning into trusted-host execution. Options with unverifiable or
conflicting security meaning are `Forbidden`.

### D10 — Readiness is live and never changes user intent

Readiness projects the latest non-expired Worker observations, not startup
state. Installation, login or capability-fingerprint changes publish a new
revision and reconcile affected publications:

- compatible selections keep their values and refresh evidence;
- unavailable login becomes `LoginRequired`;
- removed modes/options or narrowed enums become
  `ConfigurationIncompatible`;
- re-login restores readiness without restarting Awaken;
- no transition changes backend, provisioning, model, mode or option.

Automatic Assistant selection considers only `Available` backends. Exactly one
may be selected automatically; zero remains unconfigured and more than one
requires an explicit persisted choice.

### D11 — One reusable composition for CLI and Flow

The CLI-private composition moves, rather than copies, into one local-ACP
application service returning:

```text
PreparedLocalAcp
  canonical Worker profile and resolved launch argv
  installation/login/capability observations
  idempotent WorkerLocal bindings
  CredentialObservationSource
  WorkerLocalReferenceRevalidator
```

Credential ports are segregated:

- `CredentialObservationSource` publishes liveness;
- `WorkerLocalReferenceRevalidator` fences a Worker-local reference;
- `CredentialMaterialResolver` resolves actual Provider material.

Backend-owned ACP implements the first two and cannot implement material
resolution. Provider materializers implement the third. This makes absence of
local ACP credential material a type property rather than a
`MaterialKindMismatch` convention.

`awaken-cli` and `awaken-flow` install the same prepared ports into their
existing Worker builders. Flow owns no adapter inventory, default ACP,
PATH/HOME inspection, wrapper acquisition or liveness rule. A Flow allowlist may
narrow catalog entries but cannot redefine them.

The authoritative application service is
`awaken-acp-application::prepare_host_acp`. Its input is limited to the Worker workspace,
wrapper root, optional catalog allowlist and credential repository. Its
secret-free result contains observations, resolved launch argv and the composite
Worker-local liveness resolver. `awaken-cli` owns only deployment/resource
composition around this service; additional composition roots call the same
service rather than importing CLI modules.

The static dependency direction is:

```text
ACP catalog/contract
       ^
       |
Worker ACP application ----> credential repository ports
       |
       +----> protocol negotiation port ----> neutral ACP channel
       |
       v
effective Worker observation
       |
       v
Agent config/publishing ----> placement/claim
       |
       v
Runtime Host ----> ResolvedAcpLaunch ----> ACP executor/protocol
```

Neither arrow points from Runtime/executor back to discovery. The application
service can depend on the neutral protocol negotiation port, while protocol
code never depends on Worker, credential, catalog, repository or authoring
types.

### D12 — Product configuration is typed, not ambient

Adapter identity, wrapper version, launch policy, option selection, databases,
sandbox, timeout, pool and observability settings come from typed deployment
configuration or persisted domain data. Lower layers do not read them from the
process environment.

PATH, HOME, DISPLAY and OS metadata may be admitted as execution metadata. They
cannot select Provider, model, credential, adapter, mode or option.
Environment-derived Provider proposals are a duplicate authoring path and are
removed rather than synchronized with the persisted Catalog.

This is a target invariant, not a claim that every historic runtime environment
read has already been migrated. The Provider-proposal authoring path and Flow's
parallel ACP inventory/default have been removed. Sandbox fallback, warm-pool,
proxy, Kubernetes namespace and container Hand location have
been consolidated into `DeploymentConfig::sandbox`. Content capture level and
redaction are authored once in typed deployment configuration; enrollment
signing is an internally generated process capability rather than an ambient
operator secret. OAuth cache lifetime is fixed credential-domain behavior until
a per-source persisted lifetime contract exists; it is no longer process
configuration. The database-pool reader has now been removed: the CLI authors
`postgres_max_connections`, `DeploymentConfig` makes zero unrepresentable, and
the commit/dispatch adapters receive the exact resolved number. Observability is
also one typed `ResolvedDeployment::observability` policy: the existing
`OtelConfig` remains the sole exporter value object, and logging, trace-file,
OTLP trace/metric endpoints, headers and timing are injected into
`awaken-observability`; that crate has no ambient configuration reader.
Podman executable selection and local-agent stderr projection are fields of the
same typed sandbox policy. The Windows Bash tool now performs only OS metadata
discovery (`PATH`/`ProgramFiles`) instead of accepting a second product override.
The memoryd execution-plane sidecar receives store, mount, mode and shutdown
policy through explicit argv from the Kubernetes plan/local supervisor; its old
environment process protocol is removed. With those migrations, D12 is
complete: remaining non-test reads are the explicitly admitted OS metadata and
trusted-host ACP metadata projection. A2A projection persistence also consumes
the Host's injected
`DeploymentConfig::storage_dir`; the protocol adapter owns only its state
filename and serialization and no longer discovers a second path from the
environment. Worker transport identity is likewise authored as typed
`worker_id` at the product boundary and injected into `WorkerUpstream`; Runtime
Host no longer reads `AWAKEN_WORKER_ID`. PATH, HOME, DISPLAY and test gate
variables are not part of that migration.

### D13 — Request attribution and retained ACP content have one lifecycle

Data-subject identity is request-grained. Managed `POST .../events` may carry a
`user_profile_id`; the adapter projects it to neutral `DataSubjectId` on
`RunActivation`. It is not stored as a Session default and is never inferred
from an environment variable. Durable dispatch, remote claim and native, ACP or
A2A awaiting tickets retain the exact id so retry and same-Run resume cannot
change attribution.

The Runtime Host owns the one capture sink. A process-global fallback is
forbidden: the composition root installs the same sink instance that the
erasure resolver owns, and `SessionCtx::context_for` attaches it only when the
activation has an explicit subject.

Portable ACP session homes are opaque retained content. Their key is:

```text
subject scope × thread × adapter
```

Each component is mapped to a safe digest before filesystem use. Subject scope
prevents a Session that serves different users over time from merging their
opaque CLI session data. `FsSessionBlobStore` implements the neutral
`ContentEraser` port; the Host shares that canonical store with ACP recovery and
registers it in the data-subject resolver. Erasing one subject removes every
subject-scoped ACP blob without touching another subject or CLI authentication
files. Unattributed runs use a separate non-subject scope and are not falsely
claimed by a subject erasure receipt.

## Complete dynamic lifecycle

This section is the authoritative lifecycle specification. ADR-0057 owns the
Agent aggregate and secret-free publication boundary; it intentionally links
here instead of maintaining a second discovery/configuration/execution sequence.
Implementation status later in this ADR records which transitions are already
enforced and which remain release gates.

### Static ownership view

The lifecycle crosses bounded contexts through immutable values and narrow
ports. No downstream context reaches back into an upstream repository:

```text
ACP catalog/contract                         external ACP installation/login
  AcpCli + annotations                                  |
          \                                             |
           +----> Worker ACP application <--------------+
                    discovery/acquisition/liveness
                    + protocol negotiation port
                              |
                              v
                    EffectiveAcpCapabilityProfile
                    CredentialObservation
                              |
             +----------------+----------------+
             |                                 |
             v                                 v
      Agent application                 Worker advertisement
      AcpSessionConfiguration            capability + revision + TTL
             |                                 |
             +----------> publication <--------+
                          immutable demand
                          provisioning
                          capability fingerprint
                          exact credential revision
                          environment requirement
                                  |
                                  v
                         placement / claim
                                  |
                                  v
                         Runtime Host
                         revalidate + realize
                         ResolvedAcpLaunch
                                  |
                                  v
                         ACP executor/protocol
                         configure + prompt + commit
```

Ownership is deliberately split:

| Concern | Owner | Consumed by | Forbidden responsibility |
|---|---|---|---|
| protocol-external adapter facts | ACP catalog/contract | Worker ACP application | observing one host's current state |
| installation, login and negotiated capability | Worker ACP application | authoring/readiness/publication/placement | persisting Agent choices |
| backend/model/mode/native option intent | Agent aggregate | publication | copying discovered defaults or schemas |
| executable scheduling demand | immutable publication | placement, claim and Runtime Host | materializing secrets or launching a process |
| environment and resolved launch | Runtime Host/provisioning | ACP executor | scanning PATH/HOME, probing login, installing wrappers or choosing an Agent option |
| ACP wire sequencing | ACP executor/protocol | Run lifecycle | discovery inventory, repositories, placement or fallback |
| authorization | Awaken policy and realized environment | Runtime/ACP permission handling | delegation to an ACP preference |

`awaken-run-executor-acp` and `awaken-runtime-host` therefore **use** discovery
evidence but do not own discovery. Runtime Host is allowed to revalidate the
exact evidence frozen by publication and claim because that is an execution
fence, not a new selection operation. The executor receives only
`ResolvedAcpLaunch` plus the channel and validates the live handshake against
that demand. Neither layer may enumerate adapters, infer defaults, persist
capability observations or replace an unavailable selection.

### Stage contracts

The following table is the normative from-discovery-to-use contract. “Durable”
identifies the consistency boundary, not permission for another mutable source
of truth.

| Stage | Trigger | Input | Output / durable boundary | Failure and retry |
|---|---|---|---|---|
| 1. declare adapter | product release or typed deployment update | protocol-external adapter facts | one versioned catalog descriptor | invalid descriptor prevents composition; no runtime fallback |
| 2. discover installation | startup, explicit refresh, descriptor/version change | descriptor + host process probe port | exact executable/version/wrapper evidence in one Worker observation batch | typed missing/incompatible/probe failure; bounded retry |
| 3. observe login | same refresh batch, periodic liveness, pre-launch fence | descriptor status command + PATH/HOME allowlist | secret-free `CredentialObservation` with revision and TTL | login remediation or expiry; never read the login file |
| 4. negotiate capability | login available and launchable adapter | neutral channel + canonical handshake port | modes/options/protocol facts; no prompt or user Session | timeout/protocol failure; reap probe process tree |
| 5. derive effective profile | successful batch assembly | live negotiation + compatible catalog annotations | `EffectiveAcpCapabilityProfile` and deterministic fingerprint | conflict is `Incompatible`; partial batch is not published |
| 6. advertise/readiness | atomic observation publish or expiry | profile + login observation | Worker capability projection and live readiness | stale evidence becomes unselectable; prior deadline is never extended by failure |
| 7. author intent | user/API selection | current projection + generated typed contract | one Agent revision containing backend/model/native mode/options only | draft may remain unavailable; invalid type/value is rejected |
| 8. publish | explicit publish/reconcile | Agent revision + fresh verified Worker evidence | immutable provisioning, selections, fingerprint, exact credential revision and environment demand | stale/missing/incompatible evidence fails closed |
| 9. place and claim | Run enqueue/claim | immutable demand + Worker advertisement | exact Worker/incarnation/epoch binding | no exact intersection means unavailable; no substitution |
| 10. revalidate/resolve | immediately before spawn | claim + frozen demand + current host evidence | one `ResolvedAcpLaunch` in one realized Session Environment | any revision/login/wrapper/capability drift terminates the attempt |
| 11. configure and execute | resolved launch opened | channel + resolved native selections | explicit mode/options, prompt/events, committed/cancelled/failed Run | rejected setting is incompatible; never omit it and continue |
| 12. reconcile change | observation revision/expiry/login remediation/version change | latest Worker batch + affected publications | refreshed readiness and compatibility projection | preserve Agent intent; bounded retry; never auto-switch backend/model/options |

Only stages 7 and 8 persist user intent and executable publication
respectively. Stages 2–6 are expiring Worker evidence; stages 9–11 are ordinary
Run/claim state. This separation prevents a discovered default from becoming
configuration, a draft from becoming executable without publication, or a live
probe from mutating an in-flight Run.

```text
startup / refresh
  -> installation discovery
  -> exact wrapper acquisition
  -> idempotent WorkerLocal registration
  -> login observation
  -> short-lived ACP capability negotiation
  -> EffectiveAcpCapabilityProfile + fingerprint
  -> Worker advertisement and live readiness

author
  -> select Provider model or Available local ACP backend
  -> optionally select exact model, native mode and native option values
  -> validate draft against effective profile
  -> persist intent only

publish
  -> require fresh Verified profile
  -> freeze provisioning, model policy, CredentialRef/revision
  -> freeze native mode/options, capability fingerprint and environment demand

place / claim
  -> match backend + credential revision + capability + environment
  -> freeze Worker incarnation/claim epoch

launch
  -> revalidate login, revision, executable, wrapper and live capabilities
  -> realize one Session Environment
  -> construct ResolvedAcpLaunch
  -> set explicit mode/options
  -> run ordinary ACP executor
  -> commit, harvest non-secret continuity and reap

change
  -> publish new Worker observation revision
  -> update readiness and reconcile
  -> preserve authored intent; never silently substitute
```

Installation, login, capability, authored selection, publication and Run state
are orthogonal values with different owners. They must not collapse into one
mutable status column.

### State, consistency and refresh boundaries

The lifecycle has four consistency boundaries:

1. **Worker observation batch.** Installation identity, login and capability
   evidence are gathered for one adapter version and published in one heartbeat
   with one trust deadline. Credential observation runs first; capability
   negotiation reuses that exact host observation and never invokes the login
   status command again. Both result sets replace the prior batch only after
   both sources complete. A hard failure preserves the prior batch's original
   deadline and therefore cannot partially renew stale evidence.
2. **Agent aggregate revision.** Authoring persists only backend/model/mode/
   option intent. Optimistic concurrency protects edits; no Worker observation
   is copied into the aggregate.
3. **Publication revision.** Publication validates one fresh observation and
   freezes its fingerprint, exact WorkerLocal revision and environment demand.
   This is the scheduling contract.
4. **Claim/launch fence.** A claim fixes Worker incarnation and epoch.
   Immediately before spawn, the Worker repeats all mutable checks and compares
   the live handshake with the frozen demand.

Refresh is event-driven on process startup, explicit user refresh, adapter
version/wrapper change, login remediation completion and observed liveness
change, with a bounded periodic retry for transient failures. Concurrent
refreshes for the same Worker/adapter coalesce. Failures retain their typed
evidence and retry time; they do not erase user intent or publish a fallback.

The terminal states visible to callers are `Available`, `InstallationRequired`,
`LoginRequired`, `CapabilityStale`, `ConfigurationIncompatible`,
`EnvironmentUnavailable`, `LaunchFailed`, protocol failure, or the ordinary
committed/cancelled Run outcome. Every non-terminal retry is bounded and
observable.

### Lifecycle state machine

The product exposes orthogonal state instead of one ambiguous ACP status:

| Axis | States | Authoritative transition owner |
|---|---|---|
| installation | `Missing`, `Detected`, `Incompatible`, `ProbeFailed` | Worker ACP application |
| login | `Available`, `LoginRequired`, `Expired`, `Invalid`, `Disabled`, `ProbeFailed` | CLI status adapter through Worker liveness |
| capability | `Verified`, `Unavailable`, `ProbeFailed`, expired by TTL | Worker observation lifecycle |
| draft | `Valid`, `Unavailable`, `InvalidSelection` | Agent application |
| publication | `Ready`, `LoginRequired`, `CapabilityStale`, `ConfigurationIncompatible`, `EnvironmentUnavailable` | publication/readiness projection |
| Run | queued, placed, claimed, running, committed or terminal failure | ordinary Run lifecycle |

The principal transitions are:

```text
Missing --install/refresh--> Detected
Detected + login unavailable ---------> LoginRequired
Detected + login available
  + handshake success ----------------> Verified(fingerprint)
  + handshake failure ----------------> ProbeFailed

Verified --heartbeat TTL expires------> stale and unselectable
Verified --CLI/wrapper/version change-> refresh, new fingerprint
Verified --logout/revocation----------> LoginRequired
LoginRequired --external CLI login----> Available -> Verified

draft + fresh Verified profile--------> publishable
publication + exact live match--------> placeable
claim + launch-time exact match-------> running
any exact-match failure---------------> typed terminal failure, no fallback
```

`Unavailable` and `ProbeFailed` are successful negative observations and may
replace a previous `Verified` observation. Transport, repository or batch
assembly failure is different: it publishes nothing new, so the old evidence
expires naturally. This distinction prevents both false availability and
needless loss of a trustworthy negative diagnosis.

### Configuration discovery and delivery

ACP configuration has three sources with non-overlapping authority:

| Configuration fact | Source | Persisted form | Delivery |
|---|---|---|---|
| protocol-external launch/acquisition facts | versioned adapter descriptor | deployment/catalog data | wrapper argv, launch flag, config override |
| current native mode/options/choices | live ACP handshake | expiring Worker observation only | `session/set_mode`, `session/set_config_option` |
| user's chosen values | Agent aggregate | native ids and typed native values | frozen into publication, then resolved launch |

This permits discovery of model, mode, approval preference, sandbox preference,
reasoning effort and future adapter-specific settings when the ACP advertises
them. When the protocol does not advertise a setting, a versioned adapter
annotation may describe it, but it remains unverified until a supported delivery
adapter can prove it. The UI shows generic safe scalar/enumerated settings,
marks security-sensitive settings as constrained, and never treats an ACP
preference as Awaken authorization.

Uniform recognition therefore means a common descriptor envelope and state
machine, not a common list of settings. Codex may expose
`model_reasoning_effort`; Claude or another ACP may expose a different native
id, value vocabulary or no equivalent at all. The Agent and publication retain
those native identities so execution is lossless and adapter addition does not
modify Runtime or executor branching.

## Comparison with oversight-next

The reviewed `oversight-next` implementation has one table-driven
`AcpAdapterProfile` authority. It declares launch command, credential channel,
MCP delivery, model delivery and per-adapter `AcpConfigOptionSpec`; Codex, for
example, statically declares sandbox, approval and reasoning-effort options.
The execution seam looks up the profile rather than branching on adapter kind.

That design provides useful precedents:

- one data-driven adapter profile and leaf delivery interfaces;
- a common structural option type with per-adapter values;
- database-owned model choice kept separate from free ACP configuration;
- no adapter-id branching in the generic execution seam.

Awaken should reuse those principles, but not copy the implementation or its
inventory. Compared with the reviewed implementation:

| Dimension | oversight-next reviewed state | Awaken target |
|---|---|---|
| adapter differences | single static profile table | single static descriptor |
| option inventory | primarily static declarations | live ACP evidence merged with annotations |
| installed-version drift | requires table/version maintenance | fingerprinted observation and launch fence |
| unsupported native option | absent until declared | generically visible when safely typed |
| defaults | static profile may declare operational defaults | omission preserves backend default |
| launch overrides | environment-variable command/arg channels exist | typed deployment config only |
| local login custody | credential injection is profile-declared | BackendOwned CLI retains host login custody |
| security-like ACP options | may request broad runtime settings | intersected with Awaken authority/environment |

The oversight approach is simpler for a closed, pinned adapter set and has a
mature table-driven seam. Awaken's live negotiation costs an extra bounded
probe and more stale-evidence handling, but it is stronger for independently
upgraded local CLIs and heterogeneous ACPs. The combined design remains simple:
one static source for facts the protocol cannot reveal, one live source for
facts it can reveal, and no synchronization between competing catalogs.

## Failure and terminal outcomes

| Rule | Condition | Outcome |
|---|---|---|
| E1 | executable/wrapper absent | no route; installation remediation |
| E2 | login unavailable/expired | no placement/launch; login remediation |
| E3 | option only statically declared | draft allowed; publication denied |
| E4 | live/catalog schema conflict | incompatible; publish/run denied |
| E5 | capability changes compatibly | refresh evidence; keep selection |
| E6 | mode/option/value disappears | `ConfigurationIncompatible` |
| E7 | exact model delivery unproven | publication denied; no default fallback |
| E8 | BackendOwned lacks trusted Workdir | placement/realization denied |
| E9 | Provider lacks isolated environment | placement/realization denied |
| E10 | ACP preference exceeds platform policy | platform policy wins |
| E11 | any stage fails | never change backend, provisioning, model, mode or option |

Other terminal attempt outcomes include `CredentialRevisionMismatch`,
`CapabilityStale`, `EnvironmentUnavailable`, `LaunchFailed`, protocol failure
and the ordinary committed Run outcome.

## Consequences

- Each ACP may expose different native options without adding per-adapter
  authoring types or executor branches.
- Live negotiation, rather than a hand-maintained option list, is the authority
  for the installed adapter version; static descriptors retain the
  protocol-external facts ACP cannot report.
- Drafts may survive temporary Worker or login unavailability, while
  publication and launch remain fail-closed.
- Provider execution retains isolated environments while backend-owned local
  login uses trusted Workdir execution on the same Worker and executor path.
- The additional capability probe has startup/refresh cost and must be bounded,
  cached by adapter version and fingerprint, and reaped independently of Runs.
- A version change may make a published Agent temporarily incompatible; Awaken
  exposes remediation instead of silently selecting a new value.
- CLI and Flow composition become thinner because they install one reusable
  application service instead of maintaining adapter behavior.

## Redundancy removal and rollout

Duplication is removed before extending configuration:

1. make the Session-bound channel source authoritative, migrate integration
   tests/public callers and remove the overlapping source;
2. split liveness, reference revalidation and material resolution ports;
3. extract the adapter descriptor without adding a second catalog;
4. move CLI local-ACP composition into the reusable application service;
5. remove Flow ACP inventory/default configuration;
6. add live capability negotiation and the effective profile;
7. add per-Session environment selection and remove global Local forcing;
8. generate the Agent/ACP UI contract and remove handwritten model types;
9. switch readiness and Assistant selection to live Available observations;
10. remove environment Provider proposals and migrate remaining product
    environment settings to typed deployment configuration;
11. update Flow to one accessible, pushed Awaken revision and lockfile;
12. prove the real Codex and Claude host-login paths.

Implemented consolidation evidence:

- `BoundLocalChannelSource` is the sole ACP channel projection and consumes the
  Session-owned environment;
- `awaken-acp-application` owns discovery, acquisition, WorkerLocal registration
  and liveness composition;
- host process probing and observation classification live only in
  `awaken-acp-application`; Runtime Host no longer converts or imports discovery
  observations;
- `awaken-protocol-acp::negotiate_capabilities` reuses the production
  initialize/session-new state machine, sends no prompt and returns full neutral
  mode/config-option descriptors;
- `awaken-acp-contract` is the neutral channel-in/descriptor-out port between
  Worker capability probing and the official ACP protocol adapter;
- `HostAcpCapabilityNegotiator` owns the bounded PATH/HOME-only probe process;
  successful evidence becomes an `EffectiveAcpCapabilityProfile` with a
  deterministic SHA-256 fingerprint;
- `awaken-local-process` is the sole local process-group adapter used by ACP
  subprocess, Workdir and Namespace launches; Supervisor signals therefore reap
  the wrapper and descendants through one `ProcessHandle`;
- the Worker publishes credential and ACP capability observations as one
  atomic, expiring heartbeat batch; it retains the prior deadlines on a hard
  refresh failure and does not store the changing fingerprint in the immutable
  Worker manifest;
- successful AllInOne heartbeats feed one observation-fingerprint gate; split
  Control reads the same Coordinator authority through its authenticated private
  service boundary and polls that gate every five seconds; changes invoke the
  existing policy publication reconciler across scope-bound aggregates, while
  source/reconcile failures leave the fence unchanged for the next clock;
- `CredentialObservationSource` and `WorkerLocalReferenceRevalidator` are
  segregated from `CredentialMaterialResolver`;
- `CredentialPool::selection_order_at` owns `RotateSpread`,
  `derive_vendor_pool` owns the default Workspace × counterparty × backend
  derivation, and `credential_candidates` owns Exact/Pool traversal for both
  secret-free publication and permitted materialization; the former Server
  default/Exact/Pool selectors are removed;
- Runtime Host selects the Session provider from immutable
  `ModelProvisioning`: BackendOwned uses its trusted Workdir provider while
  Provider/HostExecutor retain the configured managed tier.
- reserved Assistant startup selects a local backend only when exactly one
  WorkerLocal binding is both login-Available and capability-Verified; zero or
  multiple candidates remain unseeded for explicit authoring.

Current implementation status distinguishes completed lifecycle ownership from
the remaining external/reconciliation gates:

| Lifecycle slice | Current state | Remaining work |
|---|---|---|
| adapter catalog | authoritative `AcpCli` plus neutral ACP capability contract | keep protocol-external descriptors versioned with adapter changes |
| installation/login probe | `awaken-acp-application` owns PATH/HOME-only process I/O and classification; startup, bounded periodic probing and `POST /admin/refresh-observations` share one coalescing atomic refresh operation | retain adapter-specific remediation text and the explicit refresh trigger |
| reusable CLI/Flow preparation | CLI and Flow call the same application service, binding use case, resolver and atomic launch-plan projection | no parallel inventory or composition path remains |
| capability negotiation | bounded Worker probe invokes the canonical prompt-free protocol operation; periodic and explicit event refreshes coalesce through the same Worker observation cache; all local launch paths share descendant-aware process-group reaping | retain protocol and process-group conformance tests |
| effective profile/fingerprint | typed profile and SHA-256 evidence are published dynamically with TTL and pinned by publication/placement/launch fences | retain exact-version compatibility tests |
| Agent ACP mode/options | generated discriminated selection carries default/exact model, native mode and arbitrary native option values; publication validates them | add richer widgets only as presentation concerns |
| environment selection | provisioning selects trusted/isolated provider | retain as the sole Session Environment policy |
| readiness | Flow and Console project current expiring Worker observations; a changed observation fingerprint invokes the canonical all-scope publication reconciler and failed reconciliation retries on the next heartbeat | retain live-login/version transition tests |
| automatic Assistant | exactly one Available+Verified local backend is required; zero remains unconfigured and multiple are resolved through the ordinary persisted Agent model editor | no separate default-backend preference exists |
| frontend contract | OpenAPI-generated TypeScript consumes the Rust discriminated union and live Worker ACP projection | keep generated-contract freshness gated |
| Flow | pinned to the pushed Awaken revision; one existing Worker/executor consumes the shared preparation service | keep the revision and lockfile updated atomically |
| ambient configuration | Complete: Provider proposals and Flow ACP inventory/default fields are removed; sandbox/container, content-capture, database-pool and observability policy consume typed deployment configuration; memoryd uses explicit argv; enrollment signing is internal; OAuth cache lifetime is fixed credential-domain behavior | retain the automated reader audit: only PATH/HOME/DISPLAY/OS metadata and test gates may remain ambient |
| content attribution/erasure | Managed request attribution is durable across dispatch and resume; Host-owned capture sink and subject-scoped ACP blob store share the erasure resolver; HTTP E2E proves capture + real session harvest fan-out, cross-subject isolation and idempotency | add automatic retention-expiry policy for ACP session homes |
| real host proof | installed Codex `0.145.0` completed the zero-configuration host-login release gate through real wrapper negotiation, BackendDefault publication, LLM response and committed running→idle transcript; installed Claude `2.1.220` is correctly login-required when ambient `ANTHROPIC_API_KEY` is cleared | run the same release-gated real LLM session after a persisted Claude host login is available |

Therefore discovery, generic ACP configuration, live Worker projection,
event-triggered reconciliation, Flow reuse and the real Codex persisted-login
path are implemented. A real Claude persisted-login LLM session remains the
explicit external release gate; the ADR does not equate hermetic protocol
coverage or an ambient API key with that proof.

ADR-0057's trigger-gated G2 implementation now has request attribution,
subject-scoped ACP continuity, a registered `ContentEraser`, and an Art.17 HTTP
E2E proving one erasure request removes captured content plus a genuinely
harvested session blob while preserving another subject and remaining
idempotent. Hand placement (F) and Agent disable/archive semantics (I) are
completed broader ADR-0057 phases, not alternative ACP discovery or execution
paths; their evidence and status remain owned by ADR-0057.

Managed `CodexAuthJson` is a separate product decision. BackendOwned never
enters that Provider-only artifact path. A global prohibition on creating
`auth.json` requires explicitly retiring managed Codex artifact delivery; it is
not implied by this local-login lifecycle.

## Simple-design and DDD assessment

The design has one source of truth for every fact:

| Fact | Owner |
|---|---|
| adapter protocol-external behavior | adapter catalog |
| installed version/login/live capability | Worker observation |
| selected backend/model/mode/options | Agent aggregate |
| executable demand | immutable publication |
| environment and final launch | Runtime Host |
| ACP sequencing | protocol/executor |
| authorization | Awaken permission policy |

It follows DDD and simple design because:

- bounded contexts exchange typed projections rather than sharing repositories;
- discovered capability and authored intent are different value objects;
- Provider and BackendOwned are closed, mutually exclusive variants;
- adapter differences are data, so new adapters are leaf changes;
- Runtime receives immutable execution demand, not discovery services;
- executor receives a resolved launch, not repositories or selection policy;
- native ids prevent lossy semantic conversion between adapters;
- failures are visible and closed, with no compatibility fallback track;
- CLI and Flow compose one application service rather than synchronize copies.

Implementation is complete only when the repository has one adapter catalog,
local-ACP application service, Worker observation lifecycle, generated
configuration contract, provisioning union, per-Session environment selector,
Session-bound channel source, ACP executor and permission authority.

## Test design and completion gate

Tests keep their cause/effect graph and decision-table rule beside the test.
Coverage includes:

- C1-C5 and E1-E11;
- Provider/BackendOwned × isolated/trusted environment;
- zero, one and multiple Available backends;
- option type, allowed-value and adapter-version changes;
- claim-to-launch login loss and credential revision changes;
- permission narrowing and attempted expansion;
- re-login and capability restoration without process restart;
- CLI and Flow use of the same local-ACP service;
- generated Rust/JSON Schema/TypeScript round trips;
- no environment proposal or duplicate inventory path;
- real installed Codex and Claude using their existing host login.

Hermetic fake-CLI tests prove all failure branches. Ignored/manual release tests
with real installed CLIs prove the external contract and verify that Awaken
does not read, copy or persist user login files. Documentation, relevant unit
and integration tests, final diff review and a scoped commit are required before
the implementation is complete.
