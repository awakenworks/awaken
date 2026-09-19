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

### 9. Out of scope

Moving the Postgres implementations or multi-node capability into the closed-source
repository is **not** part of this decision. It would contradict ADR-0008, ADR-0012
and ADR-0028 in this repository and the Cloud repository's ADR-0001, ADR-0040 and
ADR-0041, which place durable multi-node operation in the open product. Such a move
needs a separate product decision and its own ADR.

## Consequences

- New backend-named contract types, catch-all String errors, capability-style `Err`
  defaults, contract cycles, deeper contract chains and over-budget growth fail at
  commit time, with the crate, rule, baseline, current value and fix in the message.
- The baseline records the debt that exists today (for example 54 capability defaults
  in `awaken-session-contract` and 10 backend nouns in `awaken-provisioning-contract`).
  Each refactor that removes an instance lowers the baseline in the same commit, so
  parallel refactors merge by taking the lower number.
- `awaken-deployment-contract` no longer depends on `awaken-session-contract`. The
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
