# Configuration-To-Application And Request-To-Response Flows

This document is the single end-to-end narrative for the service split. It owns
two paths:

1. configuration to an executable application Session;
2. one Session request to its committed HTTP and SSE response.

Focused documents own their details and are linked rather than repeated:

- [publication lifecycle](config-publication-lifecycle.md);
- [Managed Deployment scheduling](managed-deployments.md);
- [Resource lifecycle](resources-memory-files-skills.md);
- [credential custody](credentials-and-vaults.md);
- [remote Worker protocol](remote-worker-protocol.md);
- [runtime commit and projection](commit-fact-projection-taxonomy.md).

The governing decision is
[ADR-0071](../adr/0071-distributed-service-boundaries-and-executable-agent-registration.md).

## Legend And Objective

- **Existing**: the current authoritative mechanism is reused.
- **Modified**: an existing owner or port changes composition or contract shape.
- **New boundary**: a local or network adapter is added without adding a domain
  model or general-purpose framework.
- **External**: infrastructure outside Awaken's data authority.

The target keeps one domain path in both deployment modes:

```text
AllInOne:     application port -> local adapter -> existing authority
Distributed:  application port -> network adapter -> same authority
```

## Static Service And Data Ownership

| Concern | Authoritative owner | Worker access |
|---|---|---|
| Agent drafts, revisions, publication history | Control | carried as a registered immutable snapshot; no Control database access |
| Agent Resource-reference authoring revisions | Control | exact resolved references in the Session manifest |
| Environment definitions, immutable revisions, and exact sandbox-policy references | Control | frozen `EnvironmentSnapshot`; no Environment database access |
| Sandbox execution policy versions | Control | exact body resolved into the executable Environment registration |
| Resource lifecycle metadata/catalog | Resources | exact resolved values plus per-kind clients |
| credential metadata, policy, and encrypted material | Control / Vault | exact claim-fenced materialization only |
| executable Agent and Environment projections | Coordinator | exact registered facts used for new Session admission |
| Deployment, DeploymentRun, Session, WorkQueue, and dispatch state | Coordinator | secret-free dispatch envelope; launch is a local Session application call |
| Session baseline and frozen Resource manifest | Coordinator | secret-free dispatch envelope |
| dispatch, Run commit, and completion | Coordinator | authenticated claim, commit, and settle APIs |
| File bytes | Resources / File aggregate | `FileContentSource` |
| Memory content, history, and CAS versions | Resources / MemoryStore aggregate | `MemorySnapshotSource` and `MemoryWritebackClient` |
| Skill bundles | Resources / Skill aggregate | `SkillBundleSource` |
| Git repository contents | external Git provider | `RepositoryRealizer` with an ephemeral credential |
| mounts, working trees, processes, and plaintext | Worker / Sandbox, ephemeral | local only |

File, Memory, custom-Skill, and Repository execution use dedicated claim-fenced
network adapters. The Worker opens no Control, Coordinator, File, Memory, Skill,
or Resource Catalog database. Its standard capability manifest is derived from
the installed adapters rather than from a marker composition value.

An owner in this table is a component boundary, not necessarily a dedicated
process. AllInOne co-locates every component. The current distributed
Coordinator process co-deploys the canonical Resources component and its File,
Memory, Skill, and Repository-verification handlers, but delegates only through
the named Resources ports. Credential selection instead crosses the authenticated
Control application boundary. Separating a provider later changes composition
and routing; it does not introduce another domain service or data model.

The crate-level static boundary is explicit:

| Component | Layer/owner | Responsibility |
|---|---|---|
| `awaken-session-application` | Coordinator application / Session | private application collaborators; exact Environment selection; repository/environment-binding consistency; one create/recovery WorkQueue projection command; lifecycle fence |
| `awaken-protocol-managed::ManagedState` | Coordinator interface / Managed wire | explicit calls into `SessionApplication`, DTO projections, public ids, event/SSE delivery; no implicit dereference or Session repository ownership |
| `awaken-runtime-host::SharedHost` | Shared host infrastructure / Runtime | one protocol-neutral execution/session substrate composed from injected ports/adapters |
| `awaken-run-ingress-http` | Coordinator interface / durable operations | HTTP translation into neutral Host durable-control methods |
| `awaken-worker-runtime` | Worker infrastructure / Worker transport | authenticated registration/lifecycle and Session-control clients |

The dynamic path is correspondingly single-track:

```text
Managed HTTP -> ManagedState wire projection -> SessionApplication
             -> exact Environment + Session repository / SharedHost ports
             -> committed Session -> one idempotent WorkQueue projection -> wire/SSE projection

Worker claim -> awaken-worker-runtime client -> Coordinator authenticated interface
             -> SessionApplication / SharedHost -> claim-fenced commit and settle
```

## Database And Migration Ownership

The deployable unit is a bounded-context migration bundle, not a physical
database and not an individual table. A production deployment may place several
bundles owned by the same service in one PostgreSQL database; each prefix keeps
its own ledger. A service must never open another service's database. AllInOne
may co-locate the same canonical bundles, but it does not gain a fifth schema or
another implementation.

Every prefix owns `<prefix>_schema_migrations` and
`<prefix>_schema_migrations_meta` in addition to the business tables below. The
ledger records bundle id, dense positive version, and checksum. IAM has four
subdomain bundle ids over the `iam` prefix and one IAM ledger; the other rows use
one bundle id per aggregate-safe scope.

| Service owner | Bundle id / prefix | Business tables (indexes, triggers, and PostgreSQL sequences omitted) |
|---|---|---|
| Control | `iam.identity`, `iam.authz`, `iam.entitlement`, `iam.audit` / `iam` | `iam_accounts`, `iam_external_identities`, `iam_sessions`, `iam_login_flows`, `iam_api_tokens`, `iam_oauth_clients`, `iam_grants`, `iam_role_bindings`, `iam_resource_edges`, `iam_orgs`, `iam_groups`, `iam_roles`, `iam_fence`, `iam_authorization_profiles`, `iam_authorization_profile_heads`, `iam_workspace_org_edges`, `iam_plans`, `iam_subscriptions`, `iam_audit_events` |
| Control | `awaken.catalog` / `catalog` | `catalog_provider`, `catalog_protocol_endpoint`, `catalog_offering`, `catalog_model_attributes` |
| Control | `awaken.credential` / `credential` | `credential_source`, `credential_secret`, `credential_pool`, `credential_creation_intent` |
| Control | `awaken.admin` / `admin` | `admin_inference_profile`, `admin_agent_resource`, `admin_webhook` |
| Control | `awaken.config` / `config` | `config_agent`, `config_publication`, `config_management_audit`, `config_management_effect`, `config_agent_revision` |
| Control | `awaken.control_data_subject` / `control_data_subject` | `control_data_subject_subject`, `control_data_subject_erasure_job` |
| Control | `awaken.env_registry` / `env_registry` | `env_registry_env`, `env_registry_create_command`, `env_registry_revision`, `env_registry_registration_intent` |
| Control | `awaken.sandbox_execution_policy` / `sandbox_execution_policy` | `sandbox_execution_policy_version`, `sandbox_execution_policy_current` |
| Coordinator | `awaken.managed_session` / `managed` | `managed_session`, `managed_lifecycle_outbox`, `managed_memory_extraction`, `managed_session_idempotency`, `managed_session_tombstone`, `managed_dream`, `managed_deployment`, `managed_deployment_run`, `managed_deployment_claim`, `managed_dream_policy` |
| Coordinator | `awaken.environment_image_build` / `environment_image_build` | `environment_image_build_job` |
| Coordinator | `awaken.work_queue` / `work_queue` | `work_queue_item` |
| Coordinator | `awaken.worker_registry` / `worker_registry` | `worker_registry_worker` |
| Coordinator | `awaken.runtime_commit`, `awaken.runtime_commit_pg` / `runtime` | `runtime_commit`, `runtime_message`, `runtime_state_command`, `runtime_event`, `runtime_run_record`, `runtime_waiting`, `runtime_thread_version`, `runtime_commit_receipt`; PostgreSQL also owns `runtime_commit_seq` |
| Coordinator | `awaken.coordinator_data_capture` / `coordinator_data_capture` | `coordinator_data_capture_captured`, `coordinator_data_capture_fence` |
| Resources | `awaken.resource_catalog` / `resource_catalog` | `resource_catalog_entry` |
| Resources | `awaken.resource_lifecycle` / `resource_lifecycle` | `resource_lifecycle_purge_intents`, `resource_lifecycle_references`, `resource_lifecycle_reclamation_fences` |
| Resources | `awaken.file_store` / `file_store` | `file_store_blob`, `file_store_file` |
| Resources | `awaken.memory_store` / `memory_store` | `memory_store_memories`, `memory_store_counters`, `memory_store_versions` |
| Resources | `awaken.skill_store` / `skill_store` | `skill_store_aggregate` |
| Worker | none | none; Worker has only ephemeral execution/cache state and receives no authority database setting |

Environment Registry and sandbox-policy persistence are Control stores. The
Environment aggregate owns its exact policy reference; the policy store owns
only immutable versions. `awaken-environment-application` owns the sole
`EnvironmentApplication` definition command path and publishes exact revisions
through `ExecutableEnvironmentRegistrar`.
Coordinator owns only the executable command log, WorkQueue, image-build, and
Session realization state. AllInOne replaces the registration transport with a
local adapter while retaining the same two authorities.

The two privacy rows are active, independently versioned authorities. Control
opens `data_subject_db`; one `DataSubjectApplication` owns User Profile,
consent/enrollment, revision-fenced aggregate updates, accountability, and
revision-fenced durable erasure checkpoints. Managed User Profile and Control consent routes
both call that same application. Control exposes only an authenticated consent
read port to Coordinator.
Coordinator opens `captured_content_db`, installs that exact adapter as the
Runtime `CaptureSink`, and exposes an authenticated erasure command back to
Control. The Coordinator erasure application also includes the configured
portable ACP session-blob adapter. Both adapters persist a subject fence and a
stable deletion receipt: a late write cannot resurrect erased content, and an
ambiguous HTTP retry returns the original count. Competing Control replicas may
replay an idempotent target effect, but checkpoint CAS gives one logical winner,
so completed targets and counts cannot regress or double. The SQL capture fence and
content delete commit atomically. AllInOne injects the same ports locally; it has
no in-memory compatibility plane.

Schema execution is deterministic:

```text
database migrate
  -> select the role's immutable migration manifest
  -> acquire only those component stores
  -> Coordinator additionally applies dispatch + worker-registry + commit bundles
  -> ledger lock + exact ledger-state read
  -> absent: execute unconditional versioned SQL, record checksum, commit
  -> current: verify checksum and perform no DDL
  -> partial/drift/unknown version: fail; never probe-and-skip an object

server process start
  -> connect_existing
  -> verify exact ledger
  -> serve, or fail closed without DDL
```

| Role | Migration manifest |
|---|---|
| `control` | Control, including Environment and sandbox policy, + Control Data Subject |
| `coordinator` | Coordinator + Coordinator Captured Content + co-deployed Resources + executable-Agent and executable-Environment projections |
| `worker` | empty; no database connection |
| `all-in-one` | Control + Control Data Subject + Coordinator + Coordinator Captured Content + Resources; executable projections are local adapters, not second durable schemas |

Conditional schema commands (`IF NOT EXISTS`, `IF EXISTS`, `CREATE OR REPLACE`),
conflict-ignore data migration, raw startup DDL, and unversioned `.sql` files are
rejected by the repository fitness check. Since this repository is still
`1.0.0-dev`, this consolidation intentionally rebases unreleased histories;
developers must recreate pre-change local databases. After the first stable
release, an applied migration is immutable and every change appends a new
version—never edits or renumbers history.

## Static Fact Publication

Agent and Environment use the same ownership pattern without sharing an
aggregate or repository:

```text
Control command
  -> validate and atomically commit an immutable revision/publication + delivery intent
  -> the domain drainer delivers the exact executable fact
  -> Coordinator command log
  -> rebuildable current + exact-revision projection
  -> new Session admission freezes the selected fact
```

Environment transitions are:

| Trigger | Control effect | Coordinator effect | Failure/recovery |
|---|---|---|---|
| create | idempotent command creates revision 1 plus one Register intent | register current; ensure one healthcheck work item | if registration is unavailable, the pending intent remains authoritative and the same drainer retries it |
| update or policy bind | atomically append the next Environment revision and Register intent; the exact policy reference advances with it | register exact revision and move current monotonically | a same revision with different facts conflicts; older exact revisions remain readable |
| archive or public delete | append one terminal archived revision and Withdraw intent; never erase history | withdraw current and remove Environment work | retry is idempotent; new Session admission fails while exact history remains auditable |
| Control restart | register built-in `env_local`; read the durable Environment intent log | replay exact registrations and terminal withdrawals through the same drainer | after the first successful rebuild, normal passes select pending intents only |

Environment-to-image realization is conditional, not a universal conversion:

```text
Cloud Environment with non-empty packages AND configured package image builder/registry
  -> registration creates one content-addressed build demand
  -> build worker resolves the base-image identity, builds, pushes, and verifies availability
  -> Session admission waits for the exact ready digest
  -> EnvironmentSnapshot.prepared_image freezes that digest
  -> Worker projects it as SandboxSpec image rootfs and clears runtime packages

Cloud Environment with non-empty packages but no configured image builder
  -> no prepared-image demand
  -> packages remain in the frozen snapshot
  -> Worker requires package-provisioning capability and installs them at sandbox creation

SelfHosted OR package-free Cloud
  -> no image-build demand
  -> Worker uses the configured/default sandbox environment directly
```

The failure analysis for this path is:

| Failure mode | S/O/D | Effect | Detection | Required elimination/mitigation |
|---|---:|---|---|---|
| definition revision commits without registration work | 9/3/7 | Environment exists but can never execute | outbox/revision parity test and FK | append revision and typed intent in one Control-store transaction |
| registration succeeds but acknowledgement is lost | 5/4/2 | duplicate delivery on retry | pending intent remains observable | Coordinator registration is fingerprint-idempotent; acknowledge only after success |
| restart rebuilds from mutable current rows | 8/3/5 | exact history or terminal order may be reconstructed incorrectly | startup recovery decision-table test | replay the immutable intent log through the same drainer |
| archive delivery fails | 9/3/4 | new Session could observe stale current availability | pending Withdraw metric/test | persist terminal Withdraw with the archive revision and retry it fail-closed |
| distinct package recipes alias a mutable image tag | 9/3/5 | wrong dependency set executes | build-key and digest assertions | key by exact registration facts plus resolved base identity; freeze verified digest |
| prepared image and packages are both realized | 8/3/4 | slow/non-deterministic double installation | Worker projection test | image rootfs and runtime package installation are mutually exclusive |
| image build is pending/unavailable | 7/4/2 | Session cannot safely start | durable build state, timeout, retry metrics | Session creation is the readiness barrier; bounded leased retry, no silent fallback |

The Control store commit is the static source of truth. A boundary failure after
that commit is reported as unavailable, never compensated by deleting or rolling
back the definition. This is the same recovery rule used by Agent publication.
One Control-owned supervisor recovers both authorities concurrently after startup
or a wake signal. For Environment it performs one full intent-log replay until
successful, then drains only pending intents. A failure preserves process serving
readiness and the durable last-known-good projections while scheduling bounded
exponential retry. Ready, pending-domain, consecutive-failure, and lag gauges
expose degradation and recovery without creating another work repository.

In an active-active Coordinator deployment, the CLI constructs one stateless
Agent-then-Environment refresh composite and shares it through the neutral
Session contract. The owning application operations invoke it at their true
consumer boundaries: Managed Session create and cold rehydration; Session
profile creation, recovery, and realization; Deployment writes and scheduler
ticks; and the existing Models, Dream-readiness, and Environment-warmup source
ports. An unchanged cursor loads no commands; an advanced cursor loads only its
ordered tail. A missing tail or cursor regression triggers full replay into a
replacement projection. Any failure is returned before the corresponding
catalog read, durable mutation, or external effect. Warm cached Session reads
and frozen execution paths do not refresh, and no route predicate or request
middleware owns correctness.

## Flow One: Configuration To Application

```mermaid
flowchart TD
    A["Client authors Agent, Resource, and credential references"]
    B["Existing: Control API admission and Workspace checks"]
    C["Existing: Agent ConfigStore"]
    D["Existing: ResourceCatalog"]
    E["Existing: credential metadata and Vault"]

    F["Client publishes Agent"]
    G["Existing: ConfigService publish validation and compilation"]
    H{"Compilation valid?"}
    I["Return validation or conflict error; no executable change"]
    J["Existing: persist StoredPublication"]

    K["Existing: ExecutableAgentRegistrar.register"]
    KA{"Deployment role"}
    KB["Existing: LocalExecutableAgentRegistrar"]
    KC["Existing: HttpExecutableAgentRegistrar"]
    KD["Existing: authenticated registration router"]
    KE["Existing: durable Postgres registrar + ExecutableAgentCatalog"]
    KF{"Registration acknowledged?"}
    KG["Return publication success with id and fingerprint"]
    KH["Return retryable unavailability; publication remains durable"]

    R["Client creates Deployment"]
    S["Existing: resolve and freeze exact published Agent version"]
    T["Existing: persist Deployment"]
    U["Return Deployment"]

    V{"Manual or scheduled trigger"}
    W["Existing: persist stable DeploymentRun"]
    X["Existing: ManagedDeploymentSessionLauncher carries deployment_run_id"]
    AA["Modified: persist SessionInitialEventPlan with the Session root"]
    AB["Existing: sole SessionInputResolver; SessionCreationIntent is the final freeze point"]
    AC["Existing: exact Environment, Skill, Resource, and credential pins"]
    AD["Existing: frozen secret-free SessionResourceManifest"]
    AE["Existing: persist Session baseline before effects"]
    AF["Modified: the Session reconciler replays the durable plan through canonical User Run, System, and Outcome owners"]
    AG["Persist DeploymentRun session_id or exact RunError"]
    AH["Return terminal DeploymentRun projection"]

    A --> B
    B --> C
    B --> D
    B --> E
    F --> G --> H
    H -- No --> I
    H -- Yes --> J --> K --> KA
    KA -- AllInOne --> KB --> KE
    KA -- Distributed --> KC --> KD --> KE
    KE --> KF
    KF -- Yes --> KG
    KF -- No --> KH
    KH -. idempotent retry or reconciliation .-> K
    KG --> R --> S --> T --> U --> V --> W --> X --> AA
    AA --> AB --> AC --> AD --> AE --> AF
    AE --> AG --> AH
```

### Publication and registration

The publication compiler, revision fence, `StoredPublication`, and
`ExecutableAgentSnapshot` are reused. The former Config Service-owned catalog
write has been removed. Control now calls one registrar; the AllInOne adapter
and distributed HTTP adapter reach the same Coordinator catalog state machine.
The PostgreSQL adapter persists the existing commands before applying that
state machine and replays them on restart. Control and Coordinator select those
adapters from role-aware configuration; Worker cannot receive their token or an
authority database binding.

Registration identity is `(workspace_id, agent_id, source_revision)` with the
snapshot fingerprint as the conflict check. The same registration may be
retried. An older exact revision may be retained but cannot move the current
pointer backwards.

### Deployment and Session creation

The existing `DeploymentSessionLauncher` is the local application seam between
Coordinator-owned Deployment and Session. Its request carries
`deployment_run_id`, the durable business identity. A repeated launch returns
the original Session id. The former remote client/router/token were deleted with
the duplicate Control-side Deployment aggregate.

Session creation performs all validation before external realization and
atomically persists the creation intent, frozen baseline, and initial Event plan
in one Session root. The sole lifecycle reconciler later executes that retained
plan through the same Session Event state machine as public Event batches.

### Terminal outcomes

| Failure point | Durable truth | Caller outcome |
|---|---|---|
| authoring or compilation rejection | existing config revision only | validation/conflict response |
| registration unavailable | `StoredPublication` remains durable | retryable service-unavailable response |
| duplicate registration | one Coordinator projection | existing success |
| Deployment validation failure | no partial Deployment mutation | public validation response |
| Session launch transport retry | one DeploymentRun and at most one Session | original success or exact failure |
| Session creation rejection | DeploymentRun stores exact `RunError` | terminal DeploymentRun response |
| failure after Session creation | Session/Run truth | DeploymentRun remains linked to the Session |

## Flow Two: Request To Complete Response

```mermaid
flowchart TD
    A["Client optionally opens Session SSE stream"]
    B["Existing: committed snapshot plus live subscription"]
    C["Client POSTs Session Events"]
    D["Existing: auth, ownership, Session state, and batch validation"]
    E{"Admitted?"}
    F["Return 4xx without a partial Event batch"]
    G["Existing: atomically retain the complete inbound Event batch"]
    H["Existing: load frozen Session baseline and Resource manifest"]
    I["Existing: prepare RunActivation"]
    J["Existing: enqueue RunDispatch with snapshot and secret-free envelopes"]

    K["Existing: authenticated Worker claim with lease epoch"]
    L["Modified/New: per-kind File, Memory, Skill, and Repository boundaries"]
    M["Added: exact Worker-private credential materialization"]
    N["Existing: Sandbox creation and repository realization"]
    O["Existing: Runtime model, tool, child Run, and HITL execution"]
    P["Existing: ThreadEventHub live observation projected per SSE connection"]

    Q["Existing: claimed commit operation"]
    R["Existing: Coordinator atomic CommitStore"]
    S{"Commit accepted?"}
    T["Return existing receipt or reject stale claim"]
    U["Existing: settle Done or Awaiting with claim epoch"]
    V["Existing: completion signal wakes Session reconciliation"]
    W["Existing: project committed facts into Managed Events"]
    X["Existing: broadcast committed SSE terminal sequence"]
    Y["Existing: POST returns the accepted public Event list"]
    Z["Existing: GET Session or Events returns committed projection"]

    AA{"Worker crashes before settlement?"}
    AB["Existing: lease expiry and higher-epoch reclaim"]
    AC["Existing: recover from committed truth and idempotent receipts"]

    A --> B
    C --> D --> E
    E -- No --> F
    E -- Yes --> G --> Y
    G --> H --> I --> J --> K --> L --> M --> N --> O
    O -. preview .-> P -.-> B
    O --> Q --> R --> S
    S -- Duplicate or stale --> T
    S -- Accepted --> U --> V --> W
    W --> X --> B
    W --> Z
    O --> AA
    AA -- Yes --> AB --> AC --> K
    AA -- No --> Q
```

### Dispatch envelope

The durable request carries existing immutable data:

- the exact `ExecutableAgentSnapshot` or its authoritative dispatch projection;
- `RunActivation` and the execution scope;
- the frozen `SessionResourceManifest`;
- the frozen Environment/runtime projection;
- exact credential references and placement requirements;
- trace context.

It never carries plaintext credentials, host paths, database handles, live
registries, authorization grants, or Sandbox handles.

### Per-kind materialization

| Resource kind | Boundary | Required semantics |
|---|---|---|
| File | `FileContentSource` | exact Workspace/File identity under the live dispatch claim, authoritative logical-to-content resolution, digest validation, read-only materialization, no write-back |
| Memory | `MemorySnapshotSource`, `MemoryMounter`, `MemoryWritebackClient` | exact config, mutable content, CAS conflict handling, recovery-safe write-back |
| Repository | `RepositoryRealizer` | exact config and credential pin, clone/fetch into Session working tree |
| Skill | `SkillBundleSource` | exact immutable bundle and capability version, no generic Resource lifecycle |
| Credential | `CredentialMaterialResolver` through a Worker-private projection or brokered adapter | exact id, revision, access, target use, Workspace, holder, and live claim |

These are network adapters over existing authority ports. They do not create a
universal Resource domain or allow Worker database access.

Before a Worker claim is committed, Run ingress decodes the frozen
`SessionRuntimeEnvelope` through the run-ingress contract and checks each MCP
attachment's exact access and selected Worker holder against that Worker's
installed materializer capabilities. Broad selection skips an incompatible
row; an exact claim returns the typed admission failure. Runtime materialization
or provider unavailability relinquishes the claim for the existing retry path,
while malformed projections, invalid policy, and exhausted realization remain
absorbing failures. The MCP generation protocol and its receipt remain the sole
durable effect authority; claim admission adds no second attachment state.

The Session control contract carries that boundary as a typed disposition.
`NotReady` means temporary backpressure, stale/conflicting/unavailable authority
means retry through the existing claim lifecycle, and absent/terminal/invalid
durable Session truth means terminal Run resolution. Worker transports and the
embedded Host consume the same classification, preventing a terminal Session
from being reinterpreted as an endlessly retryable Work slot.

For File inputs, the local and distributed paths now use the same
`FileContentSource` port. `StoreFileContentSource` resolves the public `FileId`
through the authoritative `FileCatalog` and reads its immutable blob. A remote
Worker uses `HttpFileContentSource`; the handler holds the exact claim guard,
checks the dispatch execution scope and frozen manifest, and then delegates to
that same store adapter. The returned digest is verified again on the Worker
before the existing binary-safe read-only mount is staged. The private blob key
is never accepted as caller authority.

For Memory inputs, copy materialization, crash recovery, and Recall use the
canonical atomic `snapshot_heads` operation. A distributed Worker carries a
process-local `materialization_reference` beside—not instead of—the logical
`MemoryStoreId`. `HttpMemorySnapshotSource` and `HttpMemoryWritebackClient`
decode that reference only inside the Worker boundary. The server holds the live
claim guard while proving the frozen Workspace, config version, access ceiling,
and current Resource lifecycle before delegating to `MemoryRepository`. Writable
copy teardown uses the existing CAS update and delete-if-match algorithm; a
read-only binding cannot issue a mutation.

For custom Skills, publication freezes kind, id, version, and bundle hash in the
Session manifest. Local realization uses `StoreSkillBundleSource`; a distributed
Worker uses `HttpSkillBundleSource`. The Coordinator authenticates the current
Worker incarnation, holds the claim epoch, and proves the exact binding and
Workspace are frozen before reading `SkillStore`. Both sides recompute and
verify the bundle digest. Built-in Anthropic Skills remain immutable runtime
content and do not use the Resource network boundary.

For Repository inputs, the frozen `RepositoryConfigVersion`, exact credential
pin, and existing `RepositoryRealizer` remain authoritative. A local execution
uses `CatalogRepositoryBindingVerifier`; a distributed Worker uses
`HttpRepositoryBindingVerifier` before clone/use. The Coordinator holds the live
claim and proves Workspace, Repository id, and config version against the frozen
dispatch manifest before delegating to `ResourceBindingValidator`. No Git bytes
or plaintext credential pass through this verification endpoint.

### Commit and response authority

The Worker commits under its exact claim epoch. Coordinator rejects a stale
owner, returns a durable receipt for an identical logical operation, and settles
the dispatch only after the committed boundary is known.

The public result has two surfaces:

1. `POST /v1/sessions/{id}/events` returns the admitted Event receipts after the
   processing command reaches its boundary;
2. SSE or `GET /events` returns projected Agent messages, tool activity, errors,
   and the terminal Session state.

Preview frames are best effort. Committed messages and terminal events are the
response authority, so reconnecting clients backfill the snapshot and do not
depend on Worker memory.

## Config Graph Model

Control owns the mutable graph of Agent, model, tool, plugin, Environment, and
credential facts plus Agent references to Resources and Skills. Resources owns
the referenced definitions and content. Publication and Session admission resolve
the selected graph into exact executable snapshots and a secret-free resource
manifest; neither embeds repositories or live provider objects.

`AgentId` alone is not executable identity. Session and Run behavior is pinned by
the exact snapshot, source revision, and fingerprint selected before execution.

## Changed Surface Summary

### Reused unchanged

- config revision checks, compilation, and `StoredPublication`;
- `ExecutableAgentSnapshot` and fingerprint validation;
- `DeploymentSessionLauncher` as the sole application port;
- Session creation, `SessionInputResolver`, and `SessionResourceManifest`;
- exact `CredentialMaterialResolver` semantics;
- `ScopedConfigRegistry` management-audit state and the Session lifecycle outbox;
- the existing `ResourceCatalog` implementation, now exposed by Resources;
- per-kind File, Memory, Skill, and Repository ports;
- existing Environment stores and WorkQueue state machines, now behind distinct
  Control definition and Coordinator execution contracts;
- authenticated Worker registration, claim, recovery, commit, and settle;
- committed-event projection and HTTP/SSE response behavior.

### Modified (implemented)

- publication calls a registrar rather than a process-local catalog write;
- executable catalog reads move to one Coordinator-owned projection;
- startup publication recovery reuses registration;
- Runtime Host Resource-reference, snapshot, Session-view, and Hand reads no
  longer depend on Config Service;
- disable/archive emits a monotonic withdrawal while exact history remains
  addressable;
- registration availability/storage failures return HTTP 503 after durable
  publication persistence;
- Deployment, DeploymentRun, scheduler, and Session launch now share one
  Coordinator-owned application composition;
- Environment definition/policy routes and persistence are Control-owned;
  Coordinator mounts only executable projection reads and WorkQueue routes;
- Environment revisions retain exact history, archive is terminal logical denial,
  and the obsolete policy-store Environment binding table is removed;
- process stores are role-owned groups: split Coordinator acquires no Control
  store or seal key, and split Control acquires no Session or Resources content
  store;
- split Coordinator model discovery projects current executable-Agent
  registrations instead of reading Control Catalog/Credential stores;
- Session binding, management audit, and webhook delivery consume narrow Control
  ports; AllInOne supplies local adapters and split Coordinator supplies one
  authenticated HTTP adapter;
- `ResourceAuthorities` exposes the authoritative `ResourceCatalog` beside its
  existing File, Memory, Skill, repository-verification, and lifecycle ports;
- HTTP File commands and Runtime artifact harvesting call one
  `FileApplicationService`; the former Host-side command implementation is gone;
- one `ResourcesApplication` derives File and purge application ports from one
  `ResourceAuthorities`, and one Resources router mounts every public family.

### New boundary code (implemented)

- `ExecutableAgentRegistrar`, its command/outcome/error values, and withdrawal;
- `ExecutableAgentCatalog` and `LocalExecutableAgentRegistrar`;
- `HttpExecutableAgentRegistrar` and the authenticated registration router;
- `PostgresExecutableAgentRegistrar` and its scoped command-log schema;
- `awaken-environment-contract` as the Control-owned static aggregate contract;
- `ExecutableEnvironmentRegistrar`, local/HTTP/PostgreSQL adapters, exact
  command-log projection, withdrawal, and reconciliation;
- `awaken-resource-application` as the canonical File, MemoryStore, and lifecycle
  application layer over existing repositories;
- split-role registration composition, token-file loading, catalog migration,
  and Worker database rejection;
- stable DeploymentRun-to-Session identity/fingerprint replay through the local
  application port;
- `ReferenceIndexedExecutableAgentRegistrar`, which records current Agent
  bindings in the Resources-owned `ResourceReferenceIndex` before executable
  exposure and removes them only after withdrawal;
- `WorkerCredentialFileResolver` as the exact Worker-private
  `CredentialMaterialResolver` adapter for `WorkerReference` and
  recipient-bound projected `ControlPlaneReference` material; Worker role
  composition removes the Control seal key, authority stores, and implicit
  durable Host stores;
- `worker_request_credential_file` and `worker_trust_credentials_file` as the
  role-owned projected inputs to the existing signed Worker transport, plus the
  optional Worker-owned `worker_server_ca_certificate_file` for a private
  Coordinator PKI; one authenticator instance protects dispatch, File, Memory,
  Skill, Repository, and claimed-commit routes;
- `HttpFileContentSource`, `HttpMemorySnapshotSource`,
  `HttpMemoryWritebackClient`, `HttpSkillBundleSource`, and
  `HttpRepositoryBindingVerifier`, paired with their authenticated,
  claim-fenced handlers;
- atomic Memory and Skill snapshot operations reused by both local and remote
  adapters instead of reconstructing a snapshot through parallel read paths;
- role-specific provider composition and the service-data-ownership fitness
  check in `scripts/ci/check_crate_boundaries.py`;
- authenticated Control application router/client for audit, secret-free
  credential selection, and lifecycle-fact delivery; the adapter is transport
  only and preserves the existing state machines.

No new Agent, Deployment, Session, Resource, Credential, Run, or response domain
model is introduced.

## Verification Matrix

Implementation tests derive from these cause/effect rules. Test comments must
cite the rule they cover.

| Rule | Causes | Effects |
|---|---|---|
| E1 | new publication; Coordinator available | durable publication and one executable registration; publish success |
| E2 | publication retry or startup reconciliation | no duplicate projection; same current pointer |
| E3 | same registration identity; different fingerprint | conflict; prior current remains authoritative |
| E4 | duplicate Deployment launch after timeout | one Session id for one DeploymentRun |
| E5 | invalid Session Resource batch | no Session activation or partial Resource effect |
| E6 | valid claim and exact Resource/Credential pins | materialize, execute, commit, settle, and respond |
| E7 | stale claim during credential use, commit, or write-back | all protected effects reject; higher epoch may recover |
| E8 | Worker crash before settle | reclaim and resume from committed truth without duplicate output |
| E9 | preview loss or client reconnect | committed snapshot backfills the complete terminal response |
| E10 | Memory CAS conflict | no silent overwrite; typed conflict/recovery outcome |
| E11 | a peer Coordinator commits while this replica has a warm Session cache | refresh from the committed transcript and project each Runtime message id once |
| E12 | Coordinator authority disappears or rejects the Worker incarnation | stop claim admission, drain, terminate, and restart as a fresh registered incarnation |
| E13 | public write has no idempotency identity and its response is ambiguous | do not auto-replay; reconcile/read until routing is stable, then require one explicit write decision |
| E14 | split Coordinator config includes a Control DB or seal key; split Control includes Session DB | reject before store or key acquisition |
| E15 | Control service bearer is missing or wrong | reject before audit, credential, or webhook authority is invoked |
| E16 | Control service is unavailable during lifecycle delivery | keep the existing Coordinator outbox fact pending and retry its stable identity |
| E17 | Environment definition/policy mutation commits but registration is unavailable | retain the exact Control revision, return unavailable, and reconcile through the same registrar |
| E18 | Environment archive/delete is retried | preserve terminal Control history, withdraw Coordinator current idempotently, and deny new Sessions |
| E19 | File upload/artifact command is retried with the same scoped key | return one logical File while immutable bytes may be content-deduplicated |
| E20 | logical Resource deletion or purge scheduling is retried | persist one deterministic purge intent; physical reclaim remains reference-fenced and idempotent |
| E21 | another Coordinator replica accepted an Agent or Environment registration | compare durable high-water marks and incrementally advance both projections before Session/Deployment admission |
| E22 | incremental projection tail is missing/out of order, or durable high-water is behind the local cursor | atomically full-replay the owning command log; fail admission closed if replay fails |
| E23 | Control starts while either registration boundary is unavailable | remain live and serving from last-known-good durable projections, report pending/failure/lag degradation, and retry both authoritative recoveries with bounded backoff |
| E24 | configuration requests a standalone Resources role without an independent scaling or credential-isolation topology | reject the role and keep the canonical Resources component co-deployed; create no extra migration or application path |

The concrete multi-process topology, cluster lifecycle, and fault-injection
entry points are owned by the
[K3D distributed test topology guide](../../deploy/k3d/README.md). Kustomize
overlays reuse one Postgres fixture and one Direct brain/hand fixture so these
verification rules cannot pass through a stale parallel deployment path.
The dedicated ADR-0071 overlay crosses both canonical flows through the shipped
Control and Coordinator composition roots, a database-less Worker, authenticated
registration and Control-application adapters, isolated component databases, an unavailable
Coordinator, and forced authority-role restarts. Adapter and repository tests
remain the owners of rule combinations that do not require a real cluster.

The overlay now runs two instances of the shipped `awaken-worker` artifact. Each
instance receives only its Worker identity, signed-request credential, Runtime
configuration, sandbox/cache paths, and narrow Credential/File/Memory/Skill
boundary clients. The test proves that no authority database or Control seal
configuration reaches the Worker, no Worker opens the PostgreSQL port, and one
Native run realizes exact Credential, File, Memory, and Skill pins. It also
removes the complete Coordinator tier, observes both old Worker incarnations
fail closed, and requires Kubernetes to replace them before execution resumes.
These process-level checks complete P2-B; crate checks remain defense in depth,
not the acceptance claim by themselves.

## Guardrails

G1, G2, G3, G4, G5, G6, G8, G9, G10, G13, G18, G23, G28, G29,
G37, G38, G42, and G43 in [INVARIANTS](../INVARIANTS.md).
