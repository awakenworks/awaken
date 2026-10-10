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
- Amends: [ADR-0081](0081-contract-layering-and-ratchet-guardrails.md) §7 (size is
  measured per module), §8 (bundle traits) and §9 (no optional run-time role discovery);
  the [ADR-0088](0088-bounded-context-map-and-sdk-platform-contract.md) amendment of
  2026-10-07 (the sentence that the small Session contracts are not merged);
  [ADR-0059](0059-neutral-core-and-leaf-evolution.md) §3 (where a port trait lives) and
  §6 (the `awaken-session-executor` name is retained);
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
  coordination), `*Provider` at least five. The crate `awaken-session-executor` is named
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

3. **Size is measured per module.** The ADR-0081 §7 budgets (2,000 production lines for
   a module declaring ports, 8,000 for a vocabulary module) apply to each module; a
   crate's total size is not a reason to split it. The guard's advice changes to "split
   the module".

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

8. **Guards.** Each rule above has a ratchet whose baseline may only shrink: contract
   crates on the target map; per-module size; `Option<&dyn Role>` accessors (31),
   bundle traits in contracts (18) and contract traits without a production
   implementation (4); mechanism-named types outside composition roots (33); bare
   `Runtime` outside the SDK (5); new bare `Provider`, `Coordinator` or `Host` types
   (none allowed); the noun `Turn` outside protocol adapters (0); the vocabulary check
   extended from documents to code identifiers; configuration reads outside binaries.

9. **Renames follow the code.** A name changes when its code moves or its dependency
   bundle is split, not in a separate sweep. Renames visible to awaken-cloud are
   recorded in a dated ADR-0088 amendment with their awaken-cloud impact and made
   together. `awaken-session-executor` becomes `awaken-session-executor` once realization,
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

- Each boundary has one contract crate and each new port one obvious place; the
  target is about 26 contract crates, reached as the work that separates each boundary
  finishes.
- Missing wiring becomes a compile error instead of a run-time `None`.
- Names state responsibility; the run-loop vocabulary and the mechanism-word rule are
  checked mechanically.
- The guards' baselines record today's exceptions; each shrinks as the code moves.
- ADR-0081 §7–§9, the ADR-0088 2026-10-07 amendment, ADR-0059 §3 and §6, ADR-0057's
  Assembly section and the design naming table carry pointers to this record.
