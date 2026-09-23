# ADR-0081: Contract Layering, Guard-Passing Fences, and Ratchet Guardrails

- Status: Accepted
- Date: 2026-09-17
- Builds on: ADR-0059 neutral core and leaf evolution (the four admission tests and
  the metadata-derived fitness functions); ADR-0071 distributed service boundaries
  (the Control → Coordinator registration seam); ADR-0072 Environment definition and
  execution boundary.
- Leaves unchanged: ADR-0008, ADR-0012 and ADR-0028 (the open-source product ships
  durable multi-node backends) and the hosted-platform placement recorded by the
  Cloud repository's ADR-0001, ADR-0040 and ADR-0041.

## Context

ADR-0059 decides *whether* something belongs in a contract crate: one stable domain
authority, and keeping it elsewhere would create a wrong-direction dependency. It does
not say what shape a contract should have once it exists. Nineteen crates now declare
`layer = "contract"`, and a structural survey of them found the same few shapes again
and again:

- **Size without cohesion.** `awaken-session-contract` holds about 17 000 production
  lines, and `awaken-runtime-contract` about 9 000. Vocabulary, outbound ports,
  inbound application interfaces and the Worker ↔ Control remote surface all sit in
  one crate, so every consumer compiles all of it and every change touches every
  consumer.
- **Adapter vocabulary in the neutral core.** Variants such as
  `SandboxHandlePayload::BubblewrapV1` and types such as `KubernetesPodUid` name a
  backend inside `awaken-provisioning-contract`, which contradicts ADR-0059 §1.
- **Catch-all errors.** `Storage(String)` / `Unavailable(String)` variants, and
  `SandboxError(pub String)` newtypes, give the caller nothing typed to classify.
  Callers then branch on message text.
- **Capabilities encoded as runtime errors.** More than a hundred trait methods default to
  `Err("backend does not support …")`. The default is fail-closed, so
  `check_degrading_defaults.py` rightly admits it. Still, the optional capability is
  invisible to the type system, so composition roots find a missing capability only
  at run time.
- **Check-then-act fences.** Several effect paths validate a lease or fence and then
  call the physical action in a separate step (`observe` next to
  `observe_for_effect`, `checkpoint` next to `checkpoint_for_effect`, and `Option`
  fence fallbacks in the container provider). A decorator that checks first and calls
  second has a time-of-check/time-of-use window.
- **Edges that exist for one re-export.** `awaken-deployment-contract` depended on
  `awaken-session-contract` only to re-export one fact type, which put the deployment
  contract at depth 4 in the contract graph. `awaken-environment-realization-contract`
  existed only as a second half of `awaken-executable-environment-contract`.

Each of these is cheap to add and expensive to remove. Without a mechanical gate the
refactor that removes them would be undone by ordinary feature work.

## Decision

### 1. Contract interfaces are classified by direction

Every item in a contract crate belongs to exactly one of five kinds:

| Kind | What it is | Implemented by | Example |
|---|---|---|---|
| **types** | Vocabulary values, identifiers, and their pure algebraic laws (constructors, transitions, fingerprints, including their Kani proofs) | nobody (data) | `ExecutableEnvironmentRegistration`, `EnvironmentImageBuildState` |
| **spi** | Outbound ports the domain calls | stores, providers, external systems | `ResourceRegistryRepository`, `EnvironmentImageBuildStore` |
| **api** | Inbound interfaces a context offers | the owning application/aggregate | `ResourceInventory`, `ExecutableEnvironmentRegistrar` |
| **remote** | A surface implemented on both sides of the Worker ↔ Control boundary | a local implementation and a transport client | `ClaimedSessionControl` |
| **ext** | Plugin extension points | extensions | runtime plugin traits |

A crate may hold several kinds while it is small. When a crate is split, the split
follows these kinds (vocabulary first, then spi), never file size alone.

### 2. Vocabulary admission

A name is contract vocabulary only if it survives replacing every backend. A type,
trait or variant whose name carries a backend or topology noun (`Bubblewrap`,
`Seatbelt`, `Kubernetes`, `Docker`, `Podman`, `Postgres`, `Sqlite`) is adapter
vocabulary. Its encoding belongs to the provider/adapter crate, and the contract
keeps an opaque or neutral form that the provider decodes. Comments and string
literals may name backends; type names may not.

### 3. Capability ladder, not capability errors

An optional capability is a supertrait: `trait CheckpointingSandbox: Sandbox`. A
composition root that needs it asks for the supertrait, and a provider that lacks it
does not implement it. The compiler then enforces what used to be discovered at run
time.

A default method body that *refuses on purpose* stays legitimate: a `PermissionDenied`,
an inactive inbox, a validation failure. That is the fail-closed rule of
`check_degrading_defaults.py`, and this ADR does not contradict it. The two gates
classify the same defaults on different axes:

| Default body | `check_degrading_defaults.py` | `check_contract_shape.py` |
|---|---|---|
| succeeds by calling a less-qualified sibling | rejected (downgrade) | not counted |
| `Err` saying the implementation lacks the operation | admitted (fail-closed) | counted: should be a supertrait |
| `Err` refusing a request it understands | admitted (fail-closed) | not counted |

### 4. Fence checks share the critical section of the physical action

An authority, lease or fence check must be made *in the same critical section* as the
physical effect it authorizes. The contract expresses this by passing a guard:

- The validating component returns an `EffectGuard` (a capability token that can be
  built only by validation). The effectful operation takes that guard as a parameter
  and re-asserts its fence inside the same transaction, lock or compare-and-swap that
  performs the effect.
- A decorator that checks and then delegates (`check(); inner.act()`) is forbidden for
  effects: another writer can take the fence between the two calls.
- Dual method families (`X` and `X_for_effect`) and `Option<Fence>` fallbacks are
  removed in favour of one guarded operation. An unfenced path is a separate,
  explicitly named operation that cannot be selected by passing `None`.

### 5. `FenceToken` is shared only where the algebra is shared

`FenceToken` covers exactly two authorities that have the same owner, incarnation,
epoch and expiry and the same successor rule: `SandboxEffectFence` and
`SessionRealizationLease`. Dispatch claims, attempt fences, task leases and credential
realization receipts keep their own types, because their successor and expiry rules
differ. Merging them would make one type satisfy several incompatible laws.

### 6. Split with a facade, then migrate

A contract crate is split when its cohesion or its budget (below) says so. The old
crate stays as a re-export facade, so consumers inside the workspace and pinned
consumers outside it keep compiling. Consumers then migrate to the new owner one at a
time, and the facade shrinks until it can be deleted. A re-export that only renames a
foreign type (the former `DeploymentLifecycleFact` alias) is not a facade: it
creates a dependency edge with no vocabulary of its own and is removed.

Two crates that are always consumed together and describe one lifecycle are merged
instead. `awaken-environment-realization-contract` merged into
`awaken-executable-environment-contract`: registration and image realization are two
stages of the same Coordinator-held executable Environment. They are not folded into
`awaken-environment-contract`, because that would force the provisioning dependency on
every consumer of Environment definitions.

### 7. Ratchet guardrails

`scripts/ci/check_contract_shape.py` measures every `layer = "contract"` crate against
`scripts/ci/contract_shape_baseline.json`, over the production projection (items that
are only `cfg(test)` or `cfg(kani)` and all comments removed):

| Rule | Measure | Allowed change |
|---|---|---|
| graph | contract dependency cycles; per-crate longest contract chain | no cycle; depth never grows |
| size | non-blank production lines; budget 8 000 for trait-free vocabulary crates, 2 000 otherwise | free under budget; above budget never beyond the baseline |
| backend nouns | backend/topology nouns in type, trait and variant names | count never grows |
| catch-all errors | `Storage`/`Unavailable`/`Rejected`/… variants of `*Error` enums (and `*Error` newtypes) carrying one `String` | count never grows |
| capability defaults | trait default bodies returning `Err` whose wording or variant says the capability is missing | count never grows |
| kinds | `[package.metadata.awaken] kinds` names the §1 interface kinds the crate holds; a types-only crate may not hold a port trait, a crate declaring a port kind must | hard error, no baseline |

A count above its baseline fails and names each instance with a fix. A count below its
baseline also fails until the baseline is lowered, so the gain is kept. Line counts
fail only once they fall under budget or shrink by 200 lines, so ordinary edits do not
change the baseline. Raising a number requires
`--write-baseline --reason`, and the reason is stored next to the number for review.
A merge that moves existing instances between crates records its reason the same way,
so the workspace total stays visible.
The gate runs in `lefthook` pre-commit and in the static group of
`scripts/ci/check-all.sh`.

### 8. Kept on purpose

- **`<C>`-parameterized ports** (`FileContentSource<C>` and siblings) stay. They remain
  object-safe per context type, and the parameter is exactly the dependency inversion
  that keeps `resource-contract` from depending on `run-ingress-contract`.
- **Blanket marker traits** (`ResourceRegistry`, dispatch and channel aggregates) stay.
  They are named combinations of role traits used as `dyn` objects in about thirty
  places, and a blanket `impl<T>` adds no behaviour a caller could miss.

### 9. September 2026 consolidation and remaining qualification

The September 20 consolidation established the extracted owners and passed its
Rust, static and public API checks. It did not establish complete semantic
closure: optional capability erasure, string-only error wrappers, the remaining
Session model size and complete backend conformance still require qualification.
The objective is that composition selects an exact
role before an effect, one aggregate remains authoritative for every durable
fact, and a retry carries the same identity to an exact receipt or an explicitly
indeterminate result.

September 22 follow-up qualification is in progress:

- SessionApplication uses the required Runtime supertraits directly. Its local
  projection adapter requires RealizationRuntime, while terminal drivers require
  RealizationRuntime plus TerminalCleanupRuntime. Optional discovery remains for
  partial Runtime consumers and is no longer an application execution gate.
- AnyDispatchStore no longer implements SessionContinuationOutbox unconditionally.
  Callers obtain the installed owner explicitly; a compile-fail test refuses the
  erased queue as a continuation implementation.
- FileStoreError distinguishes corrupt content, invalid configuration, absent
  authority, typed IO and indeterminate backend outcomes. Filesystem and SQL
  adapters share content integrity validation; backend corruption tests must
  reject both reads and idempotent puts without overwriting damaged evidence.
- The contract scanner now follows local string wrappers and aliases. Remaining
  opaque Resource error payloads are exposed again; they are not closed by
  the former ResourceErrorDetail name. Their classification work remains open.
  The corrected baseline records 13 pre-existing opaque payloads, the 33 lines
  needed for typed File failures, and zero remaining missing-capability defaults
  in Worker and Provisioning. This is measured debt, not a closure claim.
- Remaining Session model extraction, the complete port/backend conformance
  inventory, full formal execution and K3D execution are still open. Successful
  focused tests or tool preflight do not close these obligations.
- Resource registry dependency failures now retain server-failure classification
  at the existing Session input-resolution boundary. Protocol File lowering
  delegates to that same application operation.
- Dream process/policy CAS assertions have one owner in store-conformance and
  bindings for memory, SQLite and PostgreSQL. The new PostgreSQL binding passed
  against an ephemeral real database after extraction, including reconnect and
  exact process/policy frame preservation. The complete PostgreSQL/MinIO gate
  also passed, including credential rotation and exact-prefix PITR recovery.

#### September 22 semantic follow-up

FileCatalog now distinguishes invalid requests, corrupt durable observations,
absent configured authority and indeterminate outcomes. The existing File
application carries those failures into ResourcePurgeError without flattening
them to Storage text. Artifact publication composes the existing Resource and
Catalog errors; its HTTP adapter has one private error projection. The wire
projection carries a decision class, never database diagnostics. Contradictory
or unreadable responses remain indeterminate. A malformed receipt after an
application effect is a receipt mismatch, not evidence of a refused write.
Response-loss recovery retains the original capture revision/effect identity.

Image-build memory records now use the same RecordBackend/CAS implementation as
SQLite and PostgreSQL. Its crate-local lifecycle table owns replay, conflicting
recipes, exact lease expiry, stale completion, retry deadlines and Ready
invalidation; SQL-specific concurrency and restart tests remain at their
adapters. Sandbox policy PostgreSQL runs the existing exact-version table and
reconnect check. An explicitly configured Model Catalog or image-build database
must connect successfully; a failed connection cannot report suite success.
Stream checkpoint register laws have one owner in store-conformance and bindings
for memory, filesystem and PostgreSQL, alongside their existing crash/corruption
and dispatch-fence tests.

Skill adapters now share their encoding boundary as well as aggregate decoding.
Persisted corruption, pre-write encoding failure, typed IO causes, absent
composition authority and unknown operation outcomes remain distinct. Runtime
and Managed Skill boundaries classify availability failures consistently across
filesystem and SQL substrates; reference-index failures retain their original
Resource error. SQLite uniqueness is selected by the native constraint code,
never diagnostic text. Initial-frame validation has one owner on
SkillDefinition, shared by every backend and the reference-index decorator.
Invalid identity, head/history coordinates or bundle content are rejected before
both reference and Skill writes; a rejected request cannot leave a retention
reference behind. Durable corruption tests require failed reads and writes
to retain the original damaged frame on filesystem, SQLite and PostgreSQL.

The existing reliability testkit owns optional PostgreSQL fixture admission for
the touched suites. It adds no database driver or conformance registry: each
suite still owns its schema and behavioral rules. Absent configuration is an
explicit local skip; invalid configuration cannot become absence, and a
configured connection or schema setup must succeed. The Skill fixture's former
`.ok()?` paths are removed under this same policy.

ArtifactHarvester now returns the existing ArtifactPublicationError directly;
it no longer flattens publisher or Catalog failures into a purge-storage string.
Resource Registry separates missing dependency authority from indeterminate
repository outcomes, and both remain server availability failures at the Session
and protocol boundaries. Purge codecs distinguish corrupt stored observations
from caller validation and pre-write encoding failures. Incomplete reference
observations cannot authorize reclamation. Purge phases require a complete
claim/receipt frame, and checked generation, expiry and revision arithmetic is
performed before mutating the in-memory intent. Rejected transitions therefore
retain the exact old frame instead of leaving half-applied state behind.

Repository binding verification keeps caller-invalid, denied, concurrent,
unavailable, corrupt-authority and indeterminate observations distinct from the
Registry through the local and HTTP verifiers into Run and Host error
classification. Memory materialization references likewise distinguish invalid
encoder inputs, frozen-authority mismatch, encoding failure and corrupt wire
values. Both changes extend the existing ports and adapters; they introduce no
second binding or materialization authority.

The remaining Resource `Invalid(ResourceErrorDetail)` payloads are removed.
File Catalog, Skill, Registry and Purge failures now expose stable domain causes
for malformed records, identity/association conflicts, definition/version/bundle
failures, admission/configuration failures, claim/reference/receipt failures and
exhausted arithmetic. Protocol and Runtime adapters select recovery and status
from those variants; diagnostic text is no longer a decision input. The now
unused `ResourceErrorDetail` wrapper was deleted rather than retained as a
parallel compatibility vocabulary.

Session Resource resolution, mutation policy and realization-lease vocabulary
now belong to `awaken-session-resource-contract`; the original Session contract
is a source-compatible re-export facade for these names. The extracted crate is
types-only, remains at dependency depth 2 and keeps the Session facade at depth
3. The serialized coordinates, neutral `FenceToken` implementation and Resource
resolution behavior remain unchanged. The lease Kani harness moved with its
production kernel and is selected from the new package. Resource activation and
terminal cleanup remain with the Session aggregate: moving them exposed a
crate-private retirement plan and the Repository aggregate's private cleanup
transitions, so that candidate was rejected instead of widening internal methods
to manufacture a size reduction.

Root mutation values and their observation-preservation kernel live beside
`PersistedSession` in `session_repo/mutation.rs`; `repository_port.rs` contains
only the required persistence interface. Realization command and projection
values live in `session_realization/model.rs` while the parent owns Control
roles. This still does not complete the Session size budget. The remaining
Environment, Repository and realization model is coupled to lower MCP and
Runtime vocabulary at depth 2. A further facade split would grow the contract
chain from 3 to 4; marking that complete would require either removing the
compatibility facade or changing the accepted depth decision.

Reused owners are the existing aggregates, repositories, operation identities,
receipts, CAS implementations and test entrypoints. Modified code is limited to
error projection, image-build backend delegation, backend test bindings and the
Session Resource vocabulary owner. New code consists of necessary error cases,
one types-only contract owner and missing conformance/transport test rules;
there is no new queue, database, retry scheduler, provider hierarchy or central
conformance framework. The corrected scanner now reports zero opaque Resource
error payloads.

#### Static structure

| Classification | Authoritative owner | Resolution |
|---|---|---|
| Reused unchanged | Session aggregate/repository and realization state machine; Dispatch row, Inbox and ordinary Outbox; Resource repositories; provisioning `SandboxProvider`; existing backend conformance entrypoints | They remain the only durable fact/effect owners. No second registry, queue, lifecycle, or provider was introduced. |
| Modified | Session realization, Agent coordination, MCP realization and Environment binding ports; Run-ingress queue/checkpoint/control/outbox/settlement ports; Resource port requirements and error payloads; application and Worker composition roots | Missing capabilities are required methods or exact role traits. Composition stores only the roles a caller needs; no call catches an "unsupported" error to discover topology. Resource category variants remain stable but their opaque detail is a named type. |
| New | `awaken-session-types`, `awaken-session-budget-contract`, `awaken-session-dream-contract`, `awaken-session-event-contract`, `awaken-session-mcp-contract`, `awaken-session-work-contract`, `awaken-session-configuration-contract`, `awaken-session-state-contract`, `awaken-session-repository-contract`, `awaken-session-realization-contract`, `awaken-session-execution-contract`, `awaken-session-application-contract`, and `awaken-session-error-contract`; the HTTP copy-conflict read operation | These are cohesive Session owners with direct consumers. They add no V2 API, parallel state, or re-export facade. The HTTP operation exposes the already-authoritative `MemoryRepository::list_copy_conflicts` capability instead of manufacturing an unsupported implementation. |

The compatibility facade has been deleted. Every source and manifest now names
the owner it consumes, and the public API snapshots cover those owners directly.
The deeper visible paths in `awaken-session-execution-contract` and
`awaken-dispatch-contract` are the actual state -> realization -> runtime ->
ingress dependency chain; the ratchet records that topology with its reason so a
replacement aggregator cannot conceal it. All audited Session, Run-ingress and
Resource contracts retain zero missing-capability defaults.

The terminal and resource realization surface is deliberately a capability
ladder:

- `SessionRealizationControl` owns only begin/renew/activate/acknowledge/fail.
- Resource materialization, checkpoint Artifact publication and terminal Memory
  authorization are independent roles, combined only by
  `SessionResourceEffectControl`.
- Cleanup discovery, preparation, disposal and root Repository publication are
  independent roles, combined only by `SessionTerminalCleanupControl`.
- `ClaimedSessionRealizationControl` and
  `ClaimedSessionCoordinationControl` are separate authenticated remote roles.
- `DispatchQueue` owns ordinary Worker delivery. Remote checkpoint/recovery is
  selected through `ClaimedCheckpointTransport`; atomic Session continuation
  belongs to `SessionContinuationOutbox`; Session admission is separate from
  ordinary settlement observation.

The conformance audit does not justify a single twenty-port registry. The
authoritative suites follow the owner and lifecycle they exercise:

| Port family | Canonical suite owner | Why it is not duplicated |
|---|---|---|
| Thread commit/read, Work, Environment registry, Session/Deployment persistence, Worker directory, Resource registry | `awaken-store-conformance` | These backends share the same store-level lifecycle and trait-generic fixtures. Session and Deployment already share `session_store`; adding another Session suite would be a competing authority. |
| Dispatch/Inbox/Outbox | `awaken-dispatch-testkit` | Its claim clock, lease, reservation and continuation rules are Run-ingress behavior, not generic storage behavior. |
| Memory repository | `awaken-memory-store` shared conformance module | It owns path/generation/CAS/copy-conflict semantics and is already reused by its in-memory, filesystem, SQLite and Postgres adapters. |
| Skill repository | `awaken-skill-store` shared conformance function | Its binary bundle/version/tombstone lifecycle is crate-local and shared by its implementations. |

The existence of these suites does not establish coverage for every persistence
port. Closure requires inspecting every port's implementations and binding each
required behavior to an executed backend test, including failure and restart
cases. Existing domain suites remain authoritative; missing shared cases extend
the narrowest owner above instead of creating a competing conformance framework.

The residual store list was rechecked against actual implementations after the
facade removal. The former numeric denominator mixed backend stores with
injected effect ports, so the executable binding is the useful measure:

| Residual family | Executable authority and backend binding |
|---|---|
| Dream, stream checkpoint, image build | Existing store-conformance Dream/stream suites and the image-build crate's RecordBackend lifecycle table run on their in-repository memory and durable adapters; backend-specific restart and conflict cases remain beside each adapter. |
| File, Skill, Memory | FileStore's one set/empty-byte/round-trip law set runs on memory, FS, SQLite, PostgreSQL, and the object adapter; Skill and Memory keep their crate-local shared lifecycle suites across their implemented media. Object corruption and incomplete-upload cases remain specific to that medium. |
| Sandbox execution policy | The policy-store crate's exact-version table runs on memory, SQLite, and PostgreSQL; SQL migration and reconnect checks stay at the adapters. |
| Resource purge and reclamation | One `awaken-store-conformance::resource_lifecycle` table now runs on SQLite and PostgreSQL. The PostgreSQL binding also reopens a fresh connection to check the committed intent and reference; its configured connection cannot be treated as an absent fixture. |
| Sandbox checkpoint, terminal cleanup queue | `SandboxCheckpointStore` has no in-repository production backend to bind; it is an injected provider effect exercised through provider/Host tests. `SessionTerminalCleanupQueue` is implemented by SessionApplication, with its lifecycle tests there, and is not a separate persistence backend. Neither calls for an empty generic store suite. |

This inventory records the actual backends and executable laws. It does not
claim that a local test run without configured PostgreSQL or MinIO exercised
those media, nor that formal or K3D execution is complete.

September 23 qualification at `6772d69e54` ran the existing strict PostgreSQL/
MinIO group against throwaway substrates, including Resource lifecycle and File
laws plus exact-prefix PITR recovery. The real K3D group also passed: replicated
roles, Pod and node loss, ingress/egress/both network partitions, synchronous
PostgreSQL promotion, public cold-start/load audit, Pod-bound Native hard crash,
and NATS loss with durable polling. Fixture-image builds use their existing operator-selected network configuration. A build-network failure is separate from production provider behavior and cannot weaken a scenario assertion.

Formal closure is separate. The retained two-ID Accepted first-result Kani
lemma passes with the real resolver and every `u64` revision. Its complete
owned-mutation/two-live-output replay harness remains unproved: pinned Z3
returns ERROR, while CaDiCaL and MiniSat exhaust bounded local memory on the
revised harness (Kissat also exhausted memory before the input-tree reduction).
`formal/coverage.json` records this specific gap; static evidence linkage must
not be read as a successful full formal execution receipt. The existing test
orchestration checker continues to guard its registered Store and Dispatch
bindings. A new hand-listed backend-path checker was rejected by the scope
derivation gate and removed, so it cannot become a second, silently stale
conformance inventory.

`ContainerEnvironmentProvider` is also not a deprecated competitor to
`SandboxProvider`. `ContainerProvider<R>` already implements the neutral
`SandboxProvider` and its restore-target rung. The container projection exists
for values the Runtime Host genuinely needs but the neutral contract must not
name: shared `Arc<dyn ContainerEnvironment>` participants, retained create or
checkpoint-upload attempts, warm capacity, and post-composition Memory/Secret
installation. Both projections delegate to the same `ContainerProvider` kernels
for plan derivation, observation, adoption, restore-target acquisition and
disposal. Deleting either projection would either leak container vocabulary into
the neutral contract or force the Host to reconstruct participant state on a
second path.

#### Dynamic behavior

1. A composition root constructs a concrete implementation and installs each
   exact role required by that topology. A complete local Session runtime is
   checked for all roles at construction; a remote Worker receives only its
   claimed realization, coordination and terminal roles.
2. The ingress adapter authenticates the Worker/claim and selects the exact
   capability. Absence is rejected before the effect; it is never converted to
   an empty read, successful no-op, or generic backend failure.
3. The owning aggregate re-derives the current lease/fence and immutable intent.
   A stale owner receives the typed fenced/conflict outcome without mutation.
4. The adapter performs the physical effect and returns the exact typed receipt.
   The aggregate alone admits that receipt and advances durable state.
5. A response-loss retry presents the same operation identity. Existing CAS,
   Inbox/Outbox idempotency and receipt rules return the same committed result;
   an unavailable observation remains unknown and is not blindly replayed.

The test design for this closure is the following cause/effect table. The rows
are attached to the corresponding Rust and checker tests so this ADR is context,
not a second executable specification.

| Rule | Exact role | Current authority | Backend observation | Effect |
|---|---|---|---|---|
| C1 | present | current | success | one effect, exact receipt, one aggregate transition |
| C2 | absent | n/a | n/a | composition/adapter refusal before any write |
| C3 | present | stale or mismatched | any | fenced/conflict result, no write |
| C4 | present | current | unavailable/timeout | typed indeterminate failure; no false absence or success |
| C5 | present | current | exact replay | idempotent prior result; no duplicate effect |
| C6 | present | current | malformed or wrong response kind | typed rejection; no substituted default |

The pre-push hook now invokes the existing fail-closed public-API snapshot gate
exactly once. `check-rust.sh --push` already owned maximal all-feature clippy, so
no duplicate lint lane was added; the orchestration self-test locks both facts.
ADR-0082 remains Deferred and none of this closure changes the open-product
placement in §10.

### 10. Out of scope

Moving the Postgres implementations or multi-node capability into the closed-source
repository is **not** part of this decision. It would contradict ADR-0008, ADR-0012
and ADR-0028 in this repository and the Cloud repository's ADR-0001, ADR-0040 and
ADR-0041, which place durable multi-node operation in the open product. Such a move
needs a separate product decision and its own ADR.

## Consequences

- New backend-named contract types, catch-all String errors, capability-style `Err`
  defaults, contract cycles, deeper contract chains and over-budget growth fail at
  commit time, with the crate, rule, baseline, current value and fix in the message.
- The baseline records only current owners. The removed facade has no baseline entry;
  direct Session owner depths are explicit, while capability-default counts remain zero.
  Each refactor that removes an instance lowers the baseline in the same commit, so
  parallel refactors merge by taking the lower number.
- `awaken-deployment-contract` depends on neither a Session facade nor a Session runtime owner. The
  Deployment aggregate owns a `DeploymentLifecycleFact` that cannot carry a Session
  runtime interval. The one store that writes the shared lifecycle outbox maps it into
  the outbox record.
- Hosted Runtime role vocabulary lives in `awaken-control`, its only consumer, together
  with its Kani proof, which the strict formal gate now selects from that package. The legacy binding migration state machine is no longer public
  API; it remains only as the proof model that verifies the production migration
  target selector.
- `ResourceRegistryRepository` and the registry application interfaces are async, so
  the Postgres adapter awaits its pool directly instead of blocking a runtime thread.
- Guard-passing fences, `FenceToken`, typed sandbox errors and the opaque sandbox
  handle are decided here but land in separate changes. Until they land, their
  baselines keep the existing instances visible.
