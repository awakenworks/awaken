# ADR-0091: Architecture Rules — Contract Granularity, Trait Admission, Construction-Time Capabilities, Composition Roots and Naming

- Status: Accepted
- Date: 2026-10-09
- Builds on: [ADR-0001](0001-documentation-model-and-vocabulary-alignment.md) D3 (one
  name per concept); [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md)
  D1 (interface segregation is a means, not a goal);
  [ADR-0059](0059-neutral-core-and-leaf-evolution.md) §2.1 (a contract exists only to
  prevent a wrong-direction dependency); [ADR-0064](0064-runtime-owned-outcome-orchestration.md)
  D1 (Run, Thread, Step); [ADR-0071](0071-distributed-service-boundaries-and-executable-agent-registration.md),
  [ADR-0073](0073-session-environment-owned-hand-and-worker-capability-placement.md),
  [ADR-0065](0065-recoverable-embeddable-remote-worker.md) and
  [ADR-0084](0084-shared-worker-transport-assembly.md) (process boundaries and Worker
  capability evidence); [ADR-0088](0088-bounded-context-map-and-sdk-platform-contract.md)
  (contexts and the SDK–Platform contract); [ADR-0089](0089-session-realization-single-owner.md)
  and [ADR-0090](0090-session-state-model.md) (one decision point per state machine).
- Amended: 2026-10-24 (rules established by the crate-boundary program: Coordinator
  composition, translating protocol adapters, failure disposition, poisoned locks,
  Environment transitions, Session use-case components, the word "runtime", linear `main`)
- Amends: [ADR-0081](0081-contract-layering-and-ratchet-guardrails.md) §7 (size is
  measured per module), §8 (bundle traits) and §9 (no optional run-time role discovery);
  the [ADR-0088](0088-bounded-context-map-and-sdk-platform-contract.md) amendment of
  2026-10-07 (the sentence that the small Session contracts are not merged);
  [ADR-0059](0059-neutral-core-and-leaf-evolution.md) §3 (where a port trait lives) and
  §6 (the `awaken-runtime-host` name is retained);
  [ADR-0057](0057-unified-agent-configuration.md) "Assembly" (its enforcement);
  the `Coordinator` row of the naming table in
  [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md).

## Context

The boundary rules that matter most for the code's long-term shape are spread over
many ADRs, and some of them pull the code apart. Measured on main `14ef08382e`:

- **Contracts follow file size, not boundaries.** There are 37 contract crates with
  126 dependency edges among them and 573 edges into them; the longest contract chain
  is 9 crates (`run-ingress → session-runtime → session-realization → session-state →
  session-configuration → session-resource → credential → runtime → tool → agent`).
  One Session aggregate is spread over 14 contract crates. ADR-0081 §7 limits each
  crate to 2,000 production lines (8,000 for trait-free vocabularies), and the guard's
  advice on overflow is to split the vocabulary into its own crate; that advice, not
  the boundaries, produced most of the spread. Only 91 of 2,419 commits since August
  changed two contracts at once, so the cost is in finding and placing code, not in
  editing it.
- **Traits do not match their callers.** The contracts hold 243 traits and 822
  methods. `DispatchQueue` has 37 methods; its two production callers use 22 and 19
  of them and share 10. `session-realization-contract` has 16 traits with 23 methods,
  four of them method-less bundles; there are 18 such bundles in contracts. Four
  contract traits have no production implementation in this repository.
- **Capabilities are discovered at run time.** 31 contract methods return
  `Option<&dyn Role>`; `SessionRuntime` alone has ten. A missing role surfaces as a
  `None` branch at run time, although one object implements all eleven roles today.
  ADR-0081 §9 already installs exact roles per topology but keeps "optional discovery
  for partial Runtime consumers".
- **Names describe mechanism, not responsibility.** Outside composition roots, 33 types
  are named with mechanism words (`Host` 7, `Wiring` 4, `Ports` 4, `Adapters` 3,
  `Dependencies` 3, …). A bare type named `Runtime` exists in six crates; `*Runtime`
  has three meanings, `*Coordinator` three (a process, a store committer, multi-agent
  coordination), `*Provider` at least five. The crate `awaken-runtime-host` is named
  for where it runs, not for what it does. The run-loop vocabulary of ADR-0064 D1 is
  checked by nobody: a planning draft proposed a `SessionTurns` role.
- **Composition is half-enforced.** ADR-0057 (Proposed) names a clippy rule that does
  not exist; ADR-0088 admits `bootstrap` library crates without saying who may call
  them.

## Decision

1. **Three boundary layers, restated.** The release boundary separates the Runtime SDK
   from the Platform (ADR-0088 §4, ADR-0086). The process boundary separates Control,
   Coordinator, Worker and Sandbox (ADR-0071, ADR-0073, ADR-0059 §7); processes exchange
   messages, never shared objects, and contexts are not processes (ADR-0088 §2). Every
   state machine has exactly one decision point: the Session aggregate for the Session
   and its realization (ADR-0089, ADR-0090), the dispatch queue for work items
   (ADR-0059 §6), the Worker registry for incarnations (ADR-0084), the SDK engine for
   the run loop (ADR-0005).

2. **A contract crate is one boundary.** It must answer between whom, who implements
   and who calls. Crates on the same boundary merge; crates on different boundaries
   stay apart even when they are often used together. A small vocabulary read by many
   crates stays a leaf. A downstream contract never depends on an upstream context's
   control ports. A merge is justified by the boundary and by its effect on the
   dependency graph, never by the crate count: ADR-0081's "contract count is not a
   target" and ADR-0088's "a merge that changes the crate count but not the dependency
   graph is not made" remain the test. The 2026-10-07 sentence of ADR-0088 that the
   small Session contracts are not merged is withdrawn: its premise that the graph does
   not change counted only edges between contracts; merging them removes 46 edges from
   their users and 17 between contracts.

3. **Size is observed per module.** The ADR-0081 §7 references (2,000 production lines
   for a module declaring ports, 8,000 for a vocabulary module) remain per-module
   measurements. Under D8 they are diagnostic, not admission limits. A module or
   crate is split when its responsibilities or boundary require it, rather than
   because its line count crosses a recorded number.

4. **Trait admission.**
   - A trait exists only when its caller and implementer must not depend on each other,
     or when there are at least two real implementations.
   - One trait per caller role: its methods are the methods that role calls, decided
     from the callers, not from line counts.
   - The trait lives in the contract of its boundary and is shaped by the side that
     needs it (amends ADR-0059 §3, which only requires the contract layer).
   - Method-less bundle traits belong in composition roots, not contracts (amends
     ADR-0081 §8; existing bundles are ratcheted down).
   - Behaviour of a value type is an inherent method unless several types must be
     interchangeable.
   - Every contract trait has a production implementation.

5. **Capabilities are given at construction.** A component receives exactly the roles
   it uses as typed constructor parameters; no contract method returns
   `Option<&dyn Role>`. Topologies differ by the set of roles they construct, so the
   ADR-0081 §9 capability ladder stays, expressed in types; its allowance for optional
   run-time discovery is withdrawn. Across processes, capabilities are an immutable
   manifest, attestation and dynamic observation (ADR-0065, ADR-0073, ADR-0084).

6. **One composition root per process.** The binary crate is the composition root.
   `bootstrap`-layer libraries are fragments of a root and may be called only from
   binaries. Only binaries read deployment configuration and environment variables. A
   guard enforces this; it replaces the clippy rule ADR-0057 names.

7. **Naming.**
   0. Use the ubiquitous language of the context first, as its ADRs define it (run
      loop: ADR-0064 D1 — Thread, Run, Step, with `turn` allowed only in protocol DTOs
      and adapters; Outcome: ADR-0064; Session and realization: ADR-0066, 0074, 0075).
   1. A name is context + responsibility + a suffix with one meaning: `-contract` a
      boundary, `-application` the use-case service owning an aggregate, `-store` a
      context's persistence adapter (`awaken-store-<medium>` remains the name of a
      storage-medium backend, ADR-0039 D5), `-executor` something that executes
      (`SessionExecutor` runs Runs, `RealizationExecutor` executes realization effects),
      `protocol-*` a protocol adapter, `sandbox-*`
      an environment provider.
   2. "Runtime" means the SDK engine. Container runtimes keep the industry term, always
      qualified (`ContainerRuntime`, `DockerRuntime`).
   3. Mechanism words — Ports, Adapters, Wiring, Dependencies, Deps, Bindings, Handles,
      Services, Support, Parts, Host, Manager, Helper, Core, Base, Common, Shared, Util —
      appear only in composition roots, where wiring is the responsibility.
   4. One meaning per word across the repository: `Coordinator` is the Coordinator
      process; `Provider` is always qualified (`SandboxProvider`, `ModelProvider`,
      `SearchProvider`) and environment supply has one abstraction, `SandboxProvider`.
   5. A struct bundling dependencies is named for what its consumer does with them; if
      no such name exists it has no single responsibility and is split. A `Weak`
      back-reference must state why it is not a construction cycle.

8. **Guards.** Structural rules above have ratchets whose baselines may only shrink: contract
   crates on the target map; module dependency relationships; `Option<&dyn Role>` accessors (31),
   bundle traits in contracts (18) and contract traits without a production
   implementation (4); mechanism-named types outside composition roots (33); bare
   `Runtime` outside the SDK (5); new bare `Provider`, `Coordinator` or `Host` types
   (none allowed); the noun `Turn` outside protocol adapters (0); the vocabulary check
   extended from documents to code identifiers; configuration reads outside binaries.

   **Line measurements are diagnostic (2026-10-28).** File, function, module and
   crate line counts, including the host and extracting-realization measurements,
   remain visible for review. Exceeding a threshold or a recorded line baseline
   emits a warning and does not block a check, commit or integration. A decrease
   does not require rewriting the baseline before continuing either. Existing
   measurements and historical ceilings may be retained without raising them.
   Responsibility, ownership, dependency direction, duplicated authority, CAS and
   replay behavior, and executed tests determine whether a change is acceptable;
   structural violations and failed tests remain failures. Production code and
   tests are not moved, compressed or removed merely to meet a line count.
   Source-location and assertion anchors still have to identify the exact code
   they describe; those evidence checks are independent of file-size metrics.

   **Dependency counts are diagnostic (2026-11-01 user decision).** A registered
   crate dependency count growing or shrinking emits a warning with the old and
   new counts; equality is silent. The number does not block code admission.
   Required review reasons and registered crate presence remain hard, as do
   dependency direction, cycles, purity and actual production-use checks.
   Default/check stays read-only; tighten only lowers records; explicitly raising
   a recorded count still requires the four-field review record below.

   **Crate line ceilings are approved allowances (2026-10-31).** Every crate holds
   an approved line ceiling in `scripts/ci/modularity_baseline.json`
   `crate-line-ceiling`: its functional lines (comments, blank lines and
   `cfg(test)` code excluded) plus headroom of max(10% of the lines, 1000),
   rounded up. `awaken-runtime` gets 5% and no floor, because the SDK engine
   stays minimal. The ceiling is not a measurement that can only shrink: a crate
   at or below it is silent, and above it `_modularity_fitness.py` warns the
   author to move the code to the crate that owns it, or, when the growth is the
   crate's own responsibility, to raise the ceiling with `--write-baseline
   --reason '<four-field JSON>'`, which records lines plus headroom and appends
   the review to the crate's history. A function above 150 lines is held the
   same way at its approved length, without headroom. `--tighten` only lowers a
   ceiling to lines plus headroom. Line counts stay advisory: these warnings
   never block. `crate-budget` keeps only the review reason and the exact
   dependency count of crates above 15000 lines, both hard; the host and
   extracting-realization measurements are retired with the realization crate.

   Explicit structural baseline growth uses the existing owning guard's `--write-baseline`
   path and shared `_architecture_ratchet.baseline_cli` admission. A new `--reason`
   value is one JSON object containing exactly four nonempty string fields:
   `reason` (why the allowance must grow), `approved_by` (the actual reviewer),
   `date` (a valid `YYYY-MM-DD` date), and `remove_when` (the condition for removing
   the allowance). Duplicate fields and incomplete records are rejected before
   reading or measuring the baseline. This validates a review record's format;
   it does not authenticate an approver or waive a hard architecture rule.
   The existing writer retains the supplied text beside each affected allowance
   and appends it to prior review history. Historical free-text records remain
   readable and are not rewritten or retroactively assigned an approver.

9. **Renames follow the code.** A name changes when its code moves or its dependency
   bundle is split, not in a separate sweep. Renames visible to awaken-cloud are
   recorded in a dated ADR-0088 amendment with their awaken-cloud impact and made
   together. `awaken-runtime-host` becomes `awaken-session-executor` once realization,
   dispatch hand-off and assembly have moved out (amends ADR-0059 §6).

## Alternatives

- **Keep ADR-0081's per-crate budget and kind split.** It keeps producing crates whose
  only reason is size; the Session aggregate would stay in 14 crates.
- **Raise the per-crate budget instead.** Hides the symptom and still says nothing
  about which code belongs together.
- **Amend each ADR separately without a new record.** Naming and trait granularity have
  no existing owner; scattered amendments would leave no single place the guards cite.
- **Rename everything in one sweep now.** Most of the affected code is about to move;
  renaming first makes every importer change twice.

## Consequences

- Each boundary has one contract crate and each new port one obvious place. The
  target map follows the audited boundaries as they are separated; its original
  estimate of about 26 contracts is not an acceptance criterion.
- Missing wiring becomes a compile error instead of a run-time `None`.
- Names state responsibility; the run-loop vocabulary and the mechanism-word rule are
  checked mechanically.
- The guards' baselines record today's exceptions; each shrinks as the code moves.
- ADR-0081 §7–§9, the ADR-0088 2026-10-07 amendment, ADR-0059 §3 and §6, ADR-0057's
  Assembly section and the design naming table carry pointers to this record.


## Amendment — 2026-10-15: existing Web SDK policy boundary

`awaken-web-policy-contract` remains an independently published SDK target in
plan 9.14 rather than a pending merge into `awaken-runtime-contract`. The target
map therefore has 27 targets. This retains an existing crate and its public API
snapshot promise; it creates no contract crate. Its pure Web parser, validation,
normalization, composition and URL admission belong to the Web extension domain.
The crate's normal graph has serialization, JSON field-path and URL parsing
libraries, with no Runtime, Session application, credential, filesystem or HTTP
implementation dependency. Runtime consumes neutral facts and does not interpret
Web policy fields. The existing `awaken-tool-contract` owns the required compiler
role used by ConfigService and Session Application; Managed wire DTOs belong to
the existing protocol adapter's `tools` module. See
[the consumer and ownership inventory](../design/anthropic-alignment-and-sessions.md).

## Amendment — 2026-10-19: aggregate ownership before Contract convergence

The remaining Session convergence concerns its state, events, budget and repository
contracts. They may become modules of `awaken-session-contract` after the application
roles and their callers are stable. This is a packaging change: state transitions,
fencing, root commits and transaction boundaries keep their existing owners.
Repository, query, input and execution roles stay narrow; a shared crate is not a
shared service interface. The lightweight `awaken-session-types` stays a leaf.

Two earlier pending merges are withdrawn:

- `awaken-session-dream-contract` remains separate. Dream has its own durable job,
  state transitions and recovery protocol. Its Session reference does not make its
  writes part of the Session aggregate transaction, even though both contracts have
  the `session` bounded context.
- `awaken-session-environment-contract` remains the Environment provider SPI. Its
  `environment` bounded context implements environment capabilities; the Session
  realization contract exposes Session decisions and observations. Implementing
  those decisions does not transfer ownership of the provider SPI to Session.

The current map consequently has 29 targets and five pending sources; the actual
workspace still has 33 contract crates. These figures describe this revision, not
a reduction quota. The container/provisioning merge remains pending until the
Provider caller roles are stable. Existing SDK release boundaries, including the
Web policy kernel, remain independently published. An SDK facade may preserve a
compatible path while internal callers depend directly on the defining crate;
packaging work must retain public API and independent-consumer evidence.

`_contract_map_fitness.py` enforces a direct, unambiguous merge map: a source has one
final target, a target is never itself a source, absent targets have pending sources,
and a merge cannot cross the `bounded_context` values in workspace metadata. The
metadata and source measurements come from one cached workspace snapshot. This
context check is necessary but not sufficient: separate aggregates in one context,
such as Dream and Session, still require an explicit ownership decision. Tests pin
those decisions and exercise duplicate ownership, self-merges, chains, cycles,
stale targets and cross-context merges. All CLI baseline-writing modes, including
`--reason`, must reject an ambiguous map without changing the baseline.

## Amendment — 2026-10-20: separate Provider SPI and Container transport boundaries

The pending `awaken-container-contract` into `awaken-provisioning-contract` merge
is withdrawn after reviewing callers and dependency direction on main
`5c55874664`. Both existing crates remain independently listed targets. Sharing
the `environment` bounded context does not make them the same interface boundary.

Neutrality is relative to the published boundary. Provisioning keeps the provider
replacement rule of ADR-0081 section 2. The existing Container contract publishes
backend-specific language shared by its adapter and protocol participants; this
is a scoped exception to that section's general backend-name rule, not permission
to introduce backend clients or vocabulary into neutral Provider contracts.

| Boundary | Callers | Implementer or vocabulary owner |
|---|---|---|
| Neutral Provisioning SPI | Session realization, environment composition and callers of `SandboxProvider` | Filesystem and container providers implement the same provider role; handles and live Sandbox roles remain provider-owned |
| Container transport and broker | Container/Kubernetes adapters, Coordinator Create broker, Worker broker client and container-aware Session routing | Container owns wire/ticket/capacity values and their pure laws; the original Kubernetes ledger implements `K8sCreateBrokerEffects` |

The ordinary Cargo graphs of `awaken-provisioning-contract` and
`awaken-sandbox-local` contain no `awaken-container-contract`. Container depends
on Provisioning to interpret its failures and policy through neutral Sandbox
values; the reverse edge does not exist. Local implements the neutral provider
without Kubernetes Create requests, ledger tickets or broker effects. Container
and Kubernetes providers can depend on both contracts. Coordinator's broker
receives its existing caller-owned effects role; its real ledger/client remains
in the Kubernetes adapter and process composition selects that implementation.
Development-only real-adapter fixtures are not production dependency edges.

Combining the crates would make the neutral provider surface co-evolve with
Kubernetes wire and privileged broker operations. It would not automatically add
a Kubernetes client dependency or grant RBAC authority: the Container contract
itself has no such client, and possession of a type is not a permission. The
rejected coupling is vocabulary, caller responsibility and change scope.

Keep this direction: Container transport may consume neutral Provisioning;
Provisioning does not import Container transport or re-export its backend
vocabulary. Pure provider laws stay in Provisioning, Container wire and shared
backend policy stay in Container, physical clients/ledgers/codecs stay in their
adapters, and deployment policy is selected by the composition root. A single
`SandboxProvider` does not require a single contract for every backend concern.

The map now has 30 targets and four pending sources, all belonging to the planned
Core Session packaging change. The actual workspace still has 33 contract crates;
neither figure is a quota. Target-map regressions retain both boundaries even
with the same context and reject a disappeared boundary as an absent target,
rather than accepting it as a completed pending merge. This amendment changes
the plan and guard only; Rust APIs, dependency manifests, serialized values,
effect fencing and physical ownership do not change.

## Amendment — 2026-10-24: rules established by the crate-boundary program

The crate-boundary program (plan v4) settled eight further rules. Each is
stated once here with its reason and the guard that holds it; the design
documents linked below own the detail. A guard without a baseline holds its
count at zero.

1. **The Coordinator consumes the executor only through ports.**
   `awaken-coordinator` receives the executor's roles as one root-selected
   `ExecutionPorts` value (`crates/server/awaken-coordinator/src/execution_ports.rs`),
   so a Coordinator-only deployment links no Worker-side implementation (D5,
   D6). `scripts/ci/_dispatch_boundary_fitness.py` rule `D3` refuses the
   Session executor or any Worker-side crate in the crate's normal closure;
   rule `D4` checks the closure of the package that declares the Coordinator
   binary (`awaken-coordinator-process`), because Cargo links every target of
   a package with that package's closure. Its reviewed transitions reached
   zero and their baseline was removed; D4 refuses any Worker-side reach. The co-located
   root (`awaken-cli`) is the root meant to link executor and realization
   together ([Coordinator-only composition](../design/runtime-interface-boundaries.md#coordinator-only-composition)).

2. **Protocol adapters only translate.** A protocol crate lowers wire requests
   into application commands and projects results; the decision belongs to
   the Session application and its aggregate, so it exists once.
   `scripts/ci/_protocol_translation_fitness.py` refuses in production
   protocol code: `T1` reaching canonical Core persistence SPI origins or the
   App store-composition role (including aliases and transitive re-exports),
   while ordinary Core values remain usable; `T2`
   constructing durable write vocabulary (`IdempotencyRecord`,
   `SessionMutation`, `SessionMutationPayload::`, `SessionTombstone`,
   `ManagedLifecycleFact`); `T3` calling an aggregate decision function
   (`transition_environment`, `transition_execution`, `realization_lease_*`,
   `realization_generation_authorizes`); `T4` selecting behaviour from a raw
   lifecycle variant (`SessionExecutionState`, `ActivationState`,
   `SessionEnvironmentPhase`, `McpAttachmentState`) or reading
   `mutation_policy`; `T5` editing a Resource manifest. Domain predicates and
   wire spellings remain projection.

3. **One failure disposition per boundary.** Every realization failure has one
   answer, `RealizationError::disposition` (`Retry`, `Defer`, `Terminal`,
   `Rejected`), and the pool picks the claim release from the failure class
   alone (`awaken-dispatch-execution` `Error::claim_release`), so no caller
   re-classifies and no failure is retried without bound. `Defer` spends no
   budget; `Retry` spends one dispatch attempt through
   `DispatchExecution::retry_claim`, and the attempt that exhausts the budget
   ends `Indeterminate` through `claim_retry_exhausted`. A pending sandbox is
   bounded by its provider's provisioning deadline: `Defer` before it, `Retry`
   after it or without one. A conflict projects `RunErrorKind::Conflict`, which
   adapters answer with HTTP 409. The decision tables are
   `each_failure_class_has_one_disposition_on_the_synchronizer_and_driver_paths`
   and `a_repository_binding_conflict_projects_a_run_conflict`
   (`awaken-session-realization`), `only_a_failed_attempt_spends_the_retry_budget`
   (`awaken-dispatch-execution`) and `a_provisioning_wait_defers_only_before_the_providers_deadline`
   with its Kani proof `provisioning_defers_only_before_the_providers_deadline`
   (`awaken-session-realization-contract`); the budget is
   [ADR-0015](0015-crash-retry-budget-and-dead-letter.md)'s 2026-10-18
   amendments.

4. **A poisoned lock is a typed error.** A lock whose holder panicked guards
   possibly half-written state; an invariant lock answers it with a typed
   refusal (`RealizationLock`, `ExecutorLock`, or the owning crate's
   `From<LockPoisoned>`) or a documented fail-closed answer, never a panic;
   a leaf lock recovers it ([Lock poisoning](../design/lock-poisoning.md)).
   `clippy.toml` denies the `std::sync` lock methods in all production code
   outside `awaken-sync`, and `scripts/ci/_poisoned_lock_fitness.py` refuses
   `L1` (an `InvariantLock` acquisition followed by `.expect`/`.unwrap`) and
   `L2` (an `.expect` naming the poison) in all production code.

5. **The Session aggregate decides every Environment transition.** Changes go
   through `transition_environment` in `awaken-session-contract`, or a
   law or `SessionEnvironmentOperations` call that decides through it
   ([ADR-0090](0090-session-state-model.md),
   [transition](../design/session-environment-transition.md)).
   `scripts/ci/_single_owner_fitness.py` rule `environment-rewrite` counts
   every other rewrite (a `&mut SessionEnvironmentState`, an undecided law on
   `.environment`, an assignment to `.environment`); its baseline is only the
   test-support `awaken-store-conformance` fixtures. Rule
   `terminal-cleanup-orchestration` holds at zero any function outside
   `awaken-session-realization-driver` that orchestrates terminal cleanup
   ([ADR-0089](0089-session-realization-single-owner.md) D1).

6. **Session use cases are components.** Each use case owns its collaborators
   and implements only the roles its callers use, over the one
   `SessionMutations` CAS kernel (`awaken-session-application`
   `src/mutation.rs`); `SessionApplication` is a thin composition product
   that holds the component set, implements no role and decides nothing
   ([Session capabilities](../design/session-aggregate-and-dispatch-capabilities.md)).
   Sharing one kernel keeps one retry policy, one receipt replay and one
   fence. `scripts/ci/_session_state_ownership_fitness.py` refuses a mutable
   `PersistedSession` borrow or a direct aggregate field write in the
   application, Managed protocol and Session store; `scripts/ci/_modularity_fitness.py`
   rule `impl-spread` holds `SessionApplication`'s inherent surface to a
   shrink-only entry. No guard yet refuses a role `impl` on
   `SessionApplication`; review holds that part.

7. **"Runtime" names the SDK engine.** It is used only for the SDK agent
   engine and qualified container engines (D7.2). `scripts/ci/_naming_fitness.py`
   rules `crate-name` (a non-SDK crate name with `runtime` or a mechanism
   word) and `runtime-word` (a non-SDK public type named with `Runtime`) are
   shrink-only, beside `bare-runtime`. The crate renames and the remaining
   baselines are recorded in
   [ADR-0088](0088-bounded-context-map-and-sdk-platform-contract.md)'s
   2026-10-24 amendment.

8. **`main` is linear and has one writer.** History stays reviewable and no
   lane overwrites another's integration. The standalone `reference-transaction`
   hook installed by `scripts/ci/install_receive_hooks.py --install` runs
   `scripts/ci/check_main_linear.py`, which refuses a deletion (`L3`), a
   non-fast-forward (`L4`), a merge commit (`L5`) and any change of main
   outside the worktree named by `git config awaken.mainWriter` (`L12`).
   Baseline growth in any guard needs the complete review record
   `{reason, approved_by, date, remove_when}` of D8, admitted by
   `scripts/ci/_architecture_ratchet.py` `baseline_cli`.

### Control executable closure (2026-10-29 amendment)

`awaken-control` follows the existing Coordinator executable pattern: its thin
entrypoint belongs to `awaken-control-process` and depends only on the shared
`awaken-process-startup`. The aggregate CLI declares only `awaken`, including
AllInOne composition. Both standalone service packages retain the same service
runtime, configuration validation, migration selection, listener and drain
owners. Moving the target changes neither command-line syntax nor HTTP routes.

The lifecycle fitness check requires exactly one target in each service package
and the Control package's single startup dependency. Its closure is consequently
the same Worker-free startup closure already checked by Coordinator rule D4.
Target absence, duplicate ownership, bypassed lifecycle and an additional
Control package dependency are negative decision-table rows. The real split-role
scenario resolves Control from its owning package before deployment.


### Session reconciliation capability (2026-10-29 amendment)

The existing `SessionRepositoryReads` remains the SELECT-only caller role.
Checkpoint retention and Worker demand instead consume `SessionReconciliation`:
the original paged `reconciliation_scan` (including row-local quarantine
maintenance) and its existing `worker_demand` projection. They receive no root
create/commit or lifecycle-outbox capability. The complete Session application's
repository inherits this maintenance role because the application uses it.

SQLite and PostgreSQL implement the moved methods once on the same repository;
the default demand algorithm still scans that original maintenance index.
Process startup derives read and maintenance views from its one selected Store.
This is a coordinated Rust capability change with no new scan, root, schema,
quarantine ledger or retry owner. Root CAS and outbox atomicity are unchanged.
Compile-fail cases establish inability to commit; original durable reader and
quarantine conformance owns actual persistence effects. Cloud must migrate its
model, checkpoint and demand consumers at the same dependency pin.
