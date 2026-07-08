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
- A guardrail whose mechanism is not yet designed is marked **Target**. The
  current split, by whether the named enforcer exists in the workspace today:
  - **Active** (enforcer type and a test or CI hook exist now): G1, G3, G4, G5,
    G6, G8, G9, G13, G14, G15, G16, G18 (live-control seam only; config-publication
    coordinator and registry compiler remain target), G21, G23, G27, G28, G29,
    G30, G31, G32, G33, G34.
  - **Target** (the rule is accepted, but its enforcer is not yet built here, so
    it holds vacuously until the subsystem lands): G10/G19/G26 (public protocol
    adapters), G11 (`ContinuationGuard`), G18 (config-publication coordinator and
    registry compiler — the live-control seam of G18 is now active via
    `LiveRunControlService`), G22 (backend-binding negotiation), G25
    (observability/eval). A Target guardrail must gain a real enforcer and test in
    the same change that first builds its subsystem.
  - **Retired**: G7 (`ExecutionBackend`/`BackendProfile`). That god-seam stays
    retired. Remote tool execution returned in ADR-0044 as the narrow `ToolExecutor`
    port with a concrete driver and tests (G33), not as `ExecutionBackend`; the
    runtime still executes tools in-process by default (ADR-0007). The row is kept
    as a tombstone so G8–G34 references stay stable.
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
| G4 | Runtime builds live execution objects from its own catalog and fails closed on fingerprint/catalog mismatch. | Fingerprint consistency check (top-level vs capabilities) in `install_catalog` (`awaken-runtime/src/runtime.rs`); fingerprint mismatch check in `RunResolver::resolve` (`awaken-runtime/src/resolve.rs`) | `fingerprint_catalog_capabilities_mismatch_is_rejected` and `resolve_fails_closed_on_fingerprint_mismatch` tests |
| G5 | `RunIngress` has exactly two delivery semantics: direct `DirectRunIngress` and durable `DurableRunIngress`; durable-only operations fail closed on direct ingress. | `RunIngress` with `DirectRunIngress` / `DurableRunIngress`; durable-only ops fail closed on direct | `RunIngress` capability tests (`awaken-run-ingress`: `submit_background` fails closed on direct, succeeds on durable; durable reports `RunIngressCapabilities::DURABLE`) |
| G6 | Durable ingress behavior is additive over runtime control; durable ingress internals are not public runtime seams. | durable ingress wraps runtime control; same-source commit guard at construction | durable ingress depends on the runtime kernel (`deny.toml` direction); the worker shares one commit source for write and read by construction; durable submit/resume/recovery tests drive the same `RunExecutor`/`Runtime::resume` as direct |
| G7 | _(retired, ADR-0007)_ The runtime executes tools in-process; there is no `ExecutionBackend`/`BackendProfile` execution-placement seam. Remote/out-of-process agent execution is out of scope until a future ADR reintroduces it with a concrete driver and tests. | — | — |
| G8 | Capability configuration is segmented: descriptors are pinned/fingerprinted, execution behavior is owned by the runtime/extension and invoked in-process by id, operator policy is mutable, secrets are opaque refs, session data is runtime fact state. The operator-policy segment allows/denies a plugin by its declared `CapabilityBound` (especially `tool_gate` / `transforms` / namespace), the bound being the dry-run-`resolve`-derived projection on `PluginCapability`. | pinned `ToolDescriptor` / `ResolvedSpec` segments; behavior invoked by id; mutable operator policy keyed on `CapabilityBound` projection; opaque secret refs; session = fact state | config materialization tests; no-secret serialization tests; bound-projection on `PluginCapability`; `scripts/ci/check_crate_boundaries.py` blocks concrete builtin tool ids/symbols in neutral crates |
| G9 | Selection, capability compatibility, and health probes are never authorization; their result types carry no grant. | selection/compatibility/health result types carry no grant; `ToolGateHook` + permission policy is the only grant path | type/API checks; explicit-authorization permission tests |
| G10 | Public protocol names live only in adapters/anti-corruption layers. Runtime errors and events use neutral names. | neutral runtime error/event names; public names only in protocol adapters | grep/deny-list over runtime crates; public API snapshots (`scripts/ci/check_public_api.sh`) |
| G11 | Goal evaluation semantics live in `awaken-ext-goal` or downstream product adapters. The runtime records opaque continuation verdicts and terminal conclusions but does not interpret product outcomes. | `ContinuationGuard` records opaque verdicts; goal semantics live in `awaken-ext-goal` / adapters | extension boundary tests; replay reuses recorded verdicts |
| G13 | Store truth is single-source within a commit; outbox/protocol replay entries are committed with the authoritative state or derived after commit. | `CommitCoordinator` is the single durable write boundary; outbox/replay committed with state or derived after | transactional store tests; `ThreadCommit` serde boundary assertions; no split-store dual-write review |
| G14 | Every development-ready design names bounded context, model element, port/repository, owner, guardrail, enforcer, and first vertical slice. | ADR template (ADR-0001 D1/D2) plus this index | docs review checklist; `check_adr` and `check_invariants` hooks |
| G15 | Runtime protocol/specification material, SDK-facing schemas, examples, and conformance tests remain Apache-2.0 unless a design explicitly moves them out of the runtime boundary; code packages may carry their own package or file license metadata. | `lefthook.yml` SPDX/license hooks; `LICENSE-APACHE` / `LICENSE-MIT` plus per-package `license` metadata | license file checks; SPDX header checks |
| G16 | Neutral runtime, protocol, config, and ordinary extension code does not use product-hosting vocabulary such as `managed`; those names are restricted to product adapters or explicit boundary mapping docs. | `lefthook.yml` vocabulary deny-list over neutral crates | grep/deny-list checks; adapter boundary review |
| G18 | Configuration publication, live control, and execution stay on separate runtime-facing ports. No API may own config authoring/publication, active-run steering, loop execution, durable commit, and public projection as one controller. | separate seams: `RunResolver` / `LiveRunControl` / `RunExecutor` / `CommitCoordinatorSource`; live-control seam now enforced by `LiveRunControlService` (cancel/wake fail-closed by correlation-id, separate from `RunIngress` submission) | `cargo deny check bans` (deny.toml dependency-direction); `LiveRunControlService` fail-closed tests (`awaken-run-ingress`: cancel NotFound, wake NoSubscriber on unknown id) |
| G19 | Public protocol adapters own public DTOs, event names, headers, replay cursors, and public errors; runtime core receives only neutral activation, control, resume, and projection-source values. | protocol adapters own public DTOs; runtime gets neutral activation/control/resume values | adapter conformance tests; DTO leak snapshot tests |
| G21 | Permission policy is the only authorization path for protected runtime operations; visibility, selection, health, compatibility, and successful resource realization carry no grant. | permission policy is the sole grant path; visibility/selection/health carry no grant | no-hidden-grant tests; permission decision API checks; audit commit tests |
| G22 | Model, provider, and backend bindings are selected before execution by config or adapter policy; runtime validates the selected binding and fails closed on mismatch instead of searching for replacements. | binding selected in `ResolvedSpec`; runtime validates, fails closed, never searches | binding snapshot tests; backend negotiation tests; provider-search dependency checks |
| G23 | Config publication and runtime projection use versioned, atomic handoffs: incomplete registry installs do not replace active catalogs, and public durable projections derive from committed runtime truth. | All pre-swap validation branches in `install_catalog` return `Err` before the `active_catalog` swap; `active_catalog_is_unchanged_on_rejected_install` and `fingerprint_catalog_capabilities_mismatch_is_rejected` tests | `awaken-runtime/tests/catalog.rs` rollback and mismatch tests |
| G25 | Observability, datasets, eval reports, traces, and experiments consume committed facts or normal runtime ports; they do not rewrite runtime truth or create alternate commit paths. | observability/eval read committed facts or normal ports only | dataset lineage tests; eval harness tests; trace deletion/replay tests |
| G26 | Stable errors are owned by their source domain and mapped by adapters; runtime errors remain neutral, public error schemas stay in protocol adapters, and indeterminate remote execution is explicit. | per-domain error types; adapters map public errors; explicit `Indeterminate` result | error mapping snapshots; public DTO dependency checks; indeterminate-result tests |
| G27 | Package boundaries, license metadata, import direction, vocabulary restrictions, and ownership are enforced mechanically for every new runtime-facing design or package. | `lefthook.yml` plus `scripts/ci/` hooks (SPDX, vocabulary, ownership, role-catalog, ADR, invariants) and `deny.toml` dependency-direction | CI hook runs; `cargo deny check bans` |
| G28 | Executable snapshots are the run/thread configuration identity at the runtime-facing boundary: runtime may accept an inline `ExecutableAgentSnapshot` or resolve an `ExecutableAgentSnapshotId`, but it must not treat `AgentId` alone as complete configuration or own config CRUD/admin workflow. | `ExecutableAgentSnapshot` inline or by id; not `AgentId` alone; no config CRUD in runtime | snapshot contract API checks; inline/by-id execution tests |
| G29 | Config publication coordination and registry compilation stay outside runtime core. Runtime receives only complete catalog install requests through `RuntimeCatalogInstaller` and must not own config loading, version publication, or admin workflow. | `RuntimeCatalogInstaller` in `awaken-runtime` accepts only self-contained `RuntimeCatalogInstall` values; `awaken-config-store` (compiler) is outside `awaken-runtime` (enforced by `deny.toml` dependency direction) | `awaken-runtime/tests/catalog.rs` install transaction tests; `cargo deny check bans` (deny.toml) |
| G31 | A terminal run stores one authority: the committed run `Phase` (`Waiting \| Ended(EndCause)`). Run status, the published outcome, and the error flag are derived projections, never stored; the fault kind lives in `EndCause::Error(Failure)`, not a status string. Every run end funnels through one commit boundary that writes the `Phase` and emits the finish event once. | single `finish` boundary writes one `RunFact { phase }`; `Phase`/`EndCause`/`Failure` typing makes a second stored end-notion unrepresentable; `End` makes pause/terminus mutually exclusive | terminal projection tests (committed fault cause in fact + event; `MaxSteps` terminus; cancelled and state-conflict causes); public API snapshot of the `run` module (ADR-0005) |
| G32 | A run's authoritative current phase is the latest run-projection fact in the thread's append-only fact log; the `RunRecord` run-store read is a derived cache that equals the latest fact and is never an independent authority. The append fence is the committed fact count, not a cache row count. | replay reads the fact log (`replay_latest_phase`); `RunStore::get` projects the latest fact; `CommitRecord.sequence` is the monotonic committed count | projection test (cache equals replay-from-log); waiting test (Waiting→Ended progression retained in order, latest fact wins) (ADR-0006) |
| G30 | A plugin's actual `Contributions` are a subset of its declared `CapabilityBound`, enforced fail-closed at resolve and at catalog registration; contribution identity (registration key, `ToolDescriptor` id, and bound reference) is one value by construction. Every id-bearing kind is bounded (tools, state keys, guards, effects, scheduled actions). A dynamically discovered family declares a coarse `Namespace` ceiling and submits a resolve-time tightened sub-bound from the same snapshot, enforced `contributions ⊆ tightened ⊆ static ceiling` (`within`). `CapabilityBound` is a contribution ceiling, never authorization, and shares no type with any permission decision. | `Plugin::resolve` returns `Contributions`; `ResolvedExecutionEnv` merge runs `enforce_bound` and cross-plugin uniqueness; `Contributions::tighten_bound` + `IdBound::within`; catalog boot self-check | undeclared-contribution fail-closed tests; tightened-bound-escapes-ceiling fails closed; cross-plugin clash names both plugins; register-time self-check; identity-equivalence compile check |
| G33 | Tool execution runs behind the `ToolExecutor` port (ADR-0044): the kernel calls the port, `LocalToolExecutor` is the in-process degenerate default, and a remote hand (`awaken-tool-relay`) is a value-returning tool server that links no model client, no `CommitCoordinator`, and no store — it returns serializable `ToolOutput`; the brain commits. Indeterminate remote outcomes are explicit and idempotently re-drivable. | kernel routes `execute_tool` through `context.tool_executor` or `LocalToolExecutor`; `awaken-tool-relay` depends on `awaken-runtime-contract` only — its absence from the `awaken-runtime`/`awaken-store-*`/`awaken-provider-genai` wrapper lists in `deny.toml` makes any kernel/store/model dependency a build failure | `cargo deny check bans` (deny.toml direction); `awaken-tool-relay` relay tests (out-of-process run, unknown-tool parity, Indeterminate, idempotent re-drive, fingerprint fail-closed); `awaken-runtime` seam test (wired executor replaces in-process path); `awaken-runtime-examples` `remote_hand_e2e` (Runtime brain commits a hand's output) |
| G34 | Network topology is a serializable `ConnectionPlan` value object (ADR-0045) over the connection mechanism; it carries a `CredentialRef` only, never resolved secret material, and no product-hosting vocabulary. `ChannelFactory` maps a plan to a live channel with `InProcess` as the zero-cost degenerate arm, so one brain/hand code path spans laptop to fleet. | `awaken-connection-plan` `ConnectionPlan`/`DialAddr`/`Wiring`/`DialPolicy`/`CredentialRef`; material resolved host-side via `CredentialResolver` just before dial; `lefthook.yml` neutral-vocabulary deny-list covers the crate | no-secret-serialization test (plan serializes a ref, no `authorization`/`bearer` material); InProcess + Unix round-trip tests; `connect` fail-closed on a `Listen` plan |

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
