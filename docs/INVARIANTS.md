# Invariants

These are the architecture rules that must be enforceable in code review, tests,
dependency checks, or public API checks. If a proposed implementation needs an
exception, update the owning design (ADR) first.

This file is a guardrail **index** (see [ADR-0001](adr/0001-documentation-model-and-vocabulary-alignment.md)).
Each row names the statement, the **Enforcer** (the corpus role or mechanism that
holds the rule), and the **Validation** (the test kind that proves it).

- Enforcer/Validation name this corpus's own roles and intended tests. This
  corpus is self-contained: it does not depend on any other repository to be
  understood (ADR-0001).
- A guardrail whose mechanism is not yet designed is marked **Target**.
- The rationale (problem, decision, consequence) behind these guardrails is owned
  by [key-design-decisions.md](design/key-design-decisions.md) (D1–D22); this
  index records only the enforceable statement, enforcer, and validation, and
  does not restate that rationale.
- The `goal` worktree is a non-authoritative reference implementation for
  patterns only; it is never cited here as the enforcer.

## Guardrails

| ID | Statement | Enforcer | Validation |
|---|---|---|---|
| G1 | Durable runtime writes go through `CommitCoordinator`; protocol projections are after-commit. | `CommitCoordinator` stages durable writes; `ThreadCommit` validates the commit plan; projections derive after commit | staged-commit tests; projection-ordering tests |
| G2 | Runtime core names only runtime-domain types and the approved runtime/server port. | runtime-core public API surface plus the single gated runtime/server port | `cargo deny check bans` (deny.toml dependency-direction); public API snapshots (`scripts/ci/check_public_api.sh`) |
| G3 | Config-to-runtime input is serializable data: `ResolvedSpec` plus `CatalogFingerprint`. No live registry, `Arc<dyn ...>`, pin, scope, or product DTO crosses into the runtime core. | `ResolvedSpec` + `CatalogFingerprint` as the only config-edge values | serde boundary assertions (`tests/serde_boundary.rs`, via `cargo check --all-targets`); dependency grep; fingerprint-mismatch tests |
| G4 | Runtime builds live execution objects from its own catalog and fails closed on fingerprint/catalog mismatch. | `RuntimeCatalogInstaller` builds live objects from the installed catalog; fingerprint check fails closed | catalog fingerprint tests; descriptor resolution tests |
| G5 | `RunIngress` has exactly two delivery semantics: direct `DirectRunIngress` and durable `DurableRunIngress`; durable-only operations fail closed on direct ingress. | `RunIngress` with `DirectRunIngress` / `DurableRunIngress`; durable-only ops fail closed on direct | `RunIngress` capability tests |
| G6 | Durable ingress behavior is additive over runtime control; durable ingress internals are not public runtime seams. | durable ingress wraps runtime control; same-source commit guard at construction | public API snapshots (`scripts/ci/check_public_api.sh`); both-ingress route tests |
| G7 | Agent execution (local child run or A2A/remote agent) goes through `ExecutionBackend`, whose `BackendProfile` is checked against requirements before use; unsupported features fail closed before execution. | `ExecutionBackend` seam; `BackendProfile` capability negotiation before execution | capability negotiation tests |
| G8 | Capability configuration is segmented: descriptors are pinned/fingerprinted, execution behavior is invoked by id, operator policy is mutable, secrets are opaque refs, session data is runtime fact state. The operator-policy segment allows/denies a plugin by its declared `CapabilityBound` (especially `tool_gate` / `transforms` / namespace), the bound being the dry-run-`resolve`-derived projection on `PluginCapability`. | pinned `ToolDescriptor` / `ResolvedSpec` segments; behavior invoked by id; mutable operator policy keyed on `CapabilityBound` projection; opaque secret refs; session = fact state | config materialization tests; no-secret serialization tests; bound-projection on `PluginCapability`; `scripts/ci/check_crate_boundaries.py` blocks concrete builtin tool ids/symbols in neutral crates |
| G9 | Selection, capability compatibility, and health probes are never authorization; their result types carry no grant. | selection/compatibility/health result types carry no grant; `ToolGateHook` + permission policy is the only grant path | type/API checks; explicit-authorization permission tests |
| G10 | Public protocol names live only in adapters/anti-corruption layers. Runtime errors and events use neutral names. | neutral runtime error/event names; public names only in protocol adapters | grep/deny-list over runtime crates; public API snapshots (`scripts/ci/check_public_api.sh`) |
| G11 | Goal evaluation semantics live in `awaken-ext-goal` or downstream product adapters. The runtime records opaque continuation verdicts and terminal conclusions but does not interpret product outcomes. | `ContinuationGuard` records opaque verdicts; goal semantics live in `awaken-ext-goal` / adapters | extension boundary tests; replay reuses recorded verdicts |
| G13 | Store truth is single-source within a commit; outbox/protocol replay entries are committed with the authoritative state or derived after commit. | `CommitCoordinator` is the single durable write boundary; outbox/replay committed with state or derived after | transactional store tests; `ThreadCommit` serde boundary assertions; no split-store dual-write review |
| G14 | Every development-ready design names bounded context, model element, port/repository, owner, guardrail, enforcer, and first vertical slice. | ADR template (ADR-0001 D1/D2) plus this index | docs review checklist; `check_adr` and `check_invariants` hooks |
| G15 | Runtime protocol/specification material, SDK-facing schemas, examples, and conformance tests remain Apache-2.0 unless a design explicitly moves them out of the runtime boundary; code packages may carry their own package or file license metadata. | `lefthook.yml` SPDX/license hooks; `LICENSE-APACHE` / `LICENSE-MIT` plus per-package `license` metadata | license file checks; SPDX header checks |
| G16 | Neutral runtime, protocol, config, and ordinary extension code does not use product-hosting vocabulary such as `managed`; those names are restricted to product adapters or explicit boundary mapping docs. | `lefthook.yml` vocabulary deny-list over neutral crates | grep/deny-list checks; adapter boundary review |
| G18 | Configuration publication, live control, and execution stay on separate runtime-facing ports. No API may own config authoring/publication, active-run steering, loop execution, durable commit, and public projection as one controller. | separate seams: `RunResolver` / `LiveRunControl` / `RunExecutor` / `CommitCoordinatorSource` | `cargo deny check bans` (deny.toml dependency-direction); public API surface tests |
| G19 | Public protocol adapters own public DTOs, event names, headers, replay cursors, and public errors; runtime core receives only neutral activation, control, resume, and projection-source values. | protocol adapters own public DTOs; runtime gets neutral activation/control/resume values | adapter conformance tests; DTO leak snapshot tests |
| G21 | Permission policy is the only authorization path for protected runtime operations; visibility, selection, health, compatibility, and successful resource realization carry no grant. | permission policy is the sole grant path; visibility/selection/health carry no grant | no-hidden-grant tests; permission decision API checks; audit commit tests |
| G22 | Model, provider, and backend bindings are selected before execution by config or adapter policy; runtime validates the selected binding and fails closed on mismatch instead of searching for replacements. | binding selected in `ResolvedSpec`; runtime validates, fails closed, never searches | binding snapshot tests; backend negotiation tests; provider-search dependency checks |
| G23 | Config publication and runtime projection use versioned, atomic handoffs: incomplete registry installs do not replace active catalogs, and public durable projections derive from committed runtime truth. | `RuntimeCatalogInstaller` install is atomic; incomplete installs keep the active catalog; projections derive from committed truth | publication transaction tests; rollback tests; projection ordering/replay tests |
| G25 | Observability, datasets, eval reports, traces, and experiments consume committed facts or normal runtime ports; they do not rewrite runtime truth or create alternate commit paths. | observability/eval read committed facts or normal ports only | dataset lineage tests; eval harness tests; trace deletion/replay tests |
| G26 | Stable errors are owned by their source domain and mapped by adapters; runtime errors remain neutral, public error schemas stay in protocol adapters, and indeterminate remote execution is explicit. | per-domain error types; adapters map public errors; explicit `Indeterminate` result | error mapping snapshots; public DTO dependency checks; indeterminate-result tests |
| G27 | Package boundaries, license metadata, import direction, vocabulary restrictions, and ownership are enforced mechanically for every new runtime-facing design or package. | `lefthook.yml` plus `scripts/ci/` hooks (SPDX, vocabulary, ownership, role-catalog, ADR, invariants) and `deny.toml` dependency-direction | CI hook runs; `cargo deny check bans` |
| G28 | Executable snapshots are the run/thread configuration identity at the runtime-facing boundary: runtime may accept an inline `ExecutableAgentSnapshot` or resolve an `ExecutableAgentSnapshotId`, but it must not treat `AgentId` alone as complete configuration or own config CRUD/admin workflow. | `ExecutableAgentSnapshot` inline or by id; not `AgentId` alone; no config CRUD in runtime | snapshot contract API checks; inline/by-id execution tests |
| G29 | Config publication coordination and registry compilation stay outside runtime core. Runtime receives only complete catalog install requests through `RuntimeCatalogInstaller` and must not own config loading, version publication, or admin workflow. | `RuntimeCatalogInstaller` receives complete install requests; compiler/coordinator outside runtime | publication/install transaction tests; `cargo deny check bans` (deny.toml) |
| G31 | A terminal run stores one authority: the committed run `Phase` (`Waiting \| Ended(EndCause)`). Run status, the published outcome, and the error flag are derived projections, never stored; the fault kind lives in `EndCause::Error(Failure)`, not a status string. Every run end funnels through one commit boundary that writes the `Phase` and emits the finish event once. | single `finish` boundary writes one `RunFact { phase }`; `Phase`/`EndCause`/`Failure` typing makes a second stored end-notion unrepresentable; `End` makes pause/terminus mutually exclusive | terminal projection tests (committed fault cause in fact + event; `MaxSteps` terminus; cancelled and state-conflict causes); public API snapshot of the `run` module (ADR-0005) |
| G30 | A plugin's actual `Contributions` are a subset of its declared `CapabilityBound`, enforced fail-closed at resolve and at catalog registration; contribution identity (registration key, `ToolDescriptor` id, and bound reference) is one value by construction. Every id-bearing kind is bounded (tools, state keys, guards, effects, scheduled actions). A dynamically discovered family declares a coarse `Namespace` ceiling and submits a resolve-time tightened sub-bound from the same snapshot, enforced `contributions ⊆ tightened ⊆ static ceiling` (`within`). `CapabilityBound` is a contribution ceiling, never authorization, and shares no type with any permission decision. | `Plugin::resolve` returns `Contributions`; `ResolvedExecutionEnv` merge runs `enforce_bound` and cross-plugin uniqueness; `Contributions::tighten_bound` + `IdBound::within`; catalog boot self-check | undeclared-contribution fail-closed tests; tightened-bound-escapes-ceiling fails closed; cross-plugin clash names both plugins; register-time self-check; identity-equivalence compile check |

## DDD Review Checklist

Before implementing a design, answer these questions:

1. Which bounded context owns the behavior?
2. Is this an aggregate, entity, value object, domain service, repository, or
   adapter?
3. Which existing port carries the dependency?
4. What invariant prevents the main failure mode, and what is its enforcer?
5. What is the smallest vertical slice that exercises the behavior end to end?
6. Which names must not cross the boundary?

If the answer requires a new facade, crate, or role word, prefer using the
existing domain vocabulary (ADR-0001 D3) unless the new concept has a distinct
invariant and test.
