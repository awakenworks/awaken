# ADR-0034: The Runtime Axis Model — Pipeline Stages Versus Orthogonal Overlays

- Status: Accepted
- Date: 2026-07-01
- Depends on: ADR-0002 (resolver roles), ADR-0032 (RunnableConfig)
- Relates to: D3, D18, D21, D22; `config-to-run-execution-flow.md`,
  `runtime-interface-boundaries.md#primary-runtime-axes`

## Context

The corpus calls configuration publication, live control, and execution the
*primary* runtime axes, and activation, resolution, state, event, wait/resume,
commit, and extension the *supporting* axes that connect them without merging
authority ([D18](../design/key-design-decisions.md#d18---runtime-axes-stay-separate),
owned by [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#primary-runtime-axes)).

The word **axis** is overloaded, and the ambiguity produces a recurring design
question — "are configuration, resolution, and execution *orthogonal*?" — whose
naive answer ("yes, fully") invites over-abstraction: an attempt to make
resolution independent of configuration, when resolution must validate the
installed catalog fingerprint and therefore *depends* on it.

Two different things are both being called an axis:

1. **Pipeline stages** — configuration → resolution → execution. Sequential,
   one-way, connected by a narrow data handoff. These *cannot* be permuted.
2. **Overlay dimensions** — the independent knobs applied *within* resolution:
   the model-visible **presentation** surface, the per-tool **binding**, the
   execution **placement**, and the capability segments of
   [tool-and-capability.md](../design/tool-and-capability.md#capability-segments).
   These *are* independently variable.

This ADR does not introduce a subsystem. It records the vocabulary decision so
the two senses stop colliding, and states the precise orthogonality claim. The
axis model itself remains owned by
[runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#primary-runtime-axes)
and [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md);
this ADR links to those owners rather than restating their tables.

The reference implementations (non-authoritative, patterns only) converged on the
same shape and are cited here only as evidence, never as enforcers: a serializable
`ResolvedSpec` + catalog-fingerprint edge with a provenance-agnostic kernel, a
collapsed resolved-run value instead of a replayability type-state, and
output-named resolvers disambiguated by module path. This corpus already sits at
that endpoint with no migration debt: `ResolvedSpec` is serde data, its
`CatalogFingerprint` is derived (not authored), `RunResolver::resolve` is a thin
fail-closed fingerprint gate (G4), and no kernel type names a pin, scope, or
provenance.

## Decision

### D1: Two kinds of "axis", named distinctly

A **pipeline stage** owns a distinct authority and hands its successor a value.
An **overlay dimension** is an independently variable knob applied inside a stage.
Prose and role names must say which one they mean; "axis" alone is not enough.

Pipeline stages:

| Stage | Owns (authority) | Hands off | Must not do |
|---|---|---|---|
| Configuration | authoring, versioning, publication, pinning, provenance | serializable `ResolvedSpec` + `CatalogFingerprint` | steer a run, run a loop, commit |
| Resolution | validate the fingerprint, build the execution plan/env | `ResolvedRun` (live objects, kernel-internal) | run the loop, publish config, commit |
| Execution | run the resolved loop, stage runtime truth | committed facts/events | publish config, own pinning/provenance |

Overlay dimensions (independently variable, applied within resolution):
model-visible **presentation** (instructions + per-tool name/description/schema/
visibility), per-tool **binding** (native / relay / client-executed / delegation),
execution **placement** (behind `ToolExecutor`), and the capability segments.

### D2: The stages are authority-orthogonal, data-sequential — not "fully orthogonal"

The three stages are orthogonal in **authority** (D18: none may do another's job)
and coupled in **data-flow order** (config → resolve → execute is a one-way
pipeline; resolution depends on the installed catalog, execution depends on the
resolved plan). The only currency that crosses the config→runtime edge is the
serializable `ResolvedSpec` plus its derived `CatalogFingerprint`; no live
registry, `Arc<dyn ...>`, pin, scope, or provenance crosses it (G3). The kernel
builds live objects from its own catalog and fails closed on a fingerprint
mismatch (G4). "Fully orthogonal" is rejected: it implies permutable independence
the pipeline does not have, and licenses trying to decouple resolution from the
configuration it must validate against.

### D3: Resolution is split compile-time (heavy) and run-time (thin)

Graph resolution — model binding, plugin selection, tool merge, and overlay
application — happens at compile time and is frozen into `ResolvedSpec`
([`compile()`](../design/config-to-run-execution-flow.md#implemented-run-input-runnableconfig-adr-0032)).
Run-time resolution is a thin fingerprint gate plus live-object assembly from the
kernel's own catalog. Heavy resolution must not move into the execution loop; the
loop consumes an already-resolved plan.

### D4: Keep one thin resolver until the roles diverge; then name by output and path

ADR-0002 named three canonical resolver roles and left the naming open (its D3).
This ADR closes that: while runtime resolution is a thin fingerprint gate that one
type satisfies, do **not** split it into three trait roles — that is speculative
structure (D10). Split only when a durable-ingress *activation binding* genuinely
differs from the *resolution plane*. When splitting, name each role by its
**output** and disambiguate by **module path** (`registry` agent lookup /
`resolution` run plan / `run_ingress` activation binding), and never merge the
resolution plane with the ingress host-port. Do not adopt long prefixed role names
(D12): paths carry the layer.

### D5: The kernel fingerprint covers only the model-visible presentation

The kernel's `ResolvedSpec` and its single `CatalogFingerprint` cover only the
**presentation** surface — instructions and the tool descriptors the model sees.
Presentation overlays are applied above the kernel (at compile/resolve); the
kernel consumes the finished descriptors. Overlays are perception, never
authorization: permission remains the only grant path (G9/G21), and the
fingerprint is derived, never authored (G3/G4). **Binding and placement are not
in the kernel** (D6); when they must be replay-stable they are recorded in the
above-kernel resolved plan / `resolution_id` lineage owned by the management
side, not in the kernel's `ResolvedSpec`.

### D6: The kernel is placement-, binding-, relay-, and scheduling-agnostic

The kernel defines the `Tool` / `RawTool` ports and invokes a tool *by id*; it
does not represent *where* or *how* a call runs. A tool's execution location —
in-process, an MCP relay bound to a sandbox, a client-executed wait, or a
delegated sub-run — is decided by **which `RawTool` the host composes into the
run** and by host-installed gates (a suspending gate models client-executed use
through the existing `WaitingTicket` / `ResumeResult::ToolResult` path). A relay
is fully encapsulated inside an extension `RawTool` whose `invoke` speaks MCP; the
kernel never names a relay, an address, a binding, or a schedule. This makes
[ADR-0007](0007-runtime-owns-tool-execution.md)'s "where a call runs is a
`RawTool` detail" explicit for the axis model: the **binding** overlay dimension
of D1 is realized by host composition, not by a kernel field or a kernel-side
executor. Per-environment isolation (each sandbox its own relay-backed tools) is
therefore a host composition choice — a per-environment tool set — never a kernel
feature.

## Consequences

- Documentation and role names distinguish "pipeline stage" from "overlay
  dimension"; the flat use of "axis" for both is retired.
- The orthogonality claim is fixed as **authority-orthogonal, data-sequential**,
  closing the "are they orthogonal?" question and forbidding the
  pseudo-orthogonality that would decouple resolution from configuration.
- ADR-0002's open naming question (its D3) is resolved as guidance: no premature
  three-role split; output-plus-path naming when a split is warranted.
- No code or contract changes: this records vocabulary and the split that already
  exist (`compile()` heavy resolution; `RunResolver::resolve` thin gate;
  presentation under one fingerprint). Guardrails G3, G4, and G18 remain the
  enforcers.
- The runtime-change surface for relay / binding / placement is **zero** (D6): a
  relay tool is an extension `RawTool`; per-environment isolation is a host
  composition choice, not a kernel feature. The `ToolExecutor` port stays unused
  until a cross-cutting execution policy (dedupe / exactly-once / correlation)
  needs one — YAGNI until then.

## Alternatives considered

- **A new `runtime-axes.md` design doc.** Rejected: the axis model is already
  owned by `runtime-interface-boundaries.md` and `config-to-run-execution-flow.md`
  (D18). A second document would duplicate an owner and violate the one-owner rule
  (ADR-0001). A decision record that links to the owners is the correct vehicle.
- **Declare the three stages fully orthogonal.** Rejected (D2): they are a
  one-way data pipeline; only their authorities are orthogonal.
- **Split the three resolver roles now.** Rejected (D4): speculative until the
  roles diverge; the current thin gate is the simpler design.
- **Inject a `ToolExecutor` or route bindings inside the kernel.** Rejected (D6):
  the kernel invokes `RawTool` by id and is placement-agnostic; relay, alias, and
  per-environment routing live inside the `RawTool` the host composes, so the
  kernel needs no relay awareness and no executor indirection.

## References

- [key-design-decisions.md](../design/key-design-decisions.md) — D3, D18, D21, D22.
- [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md) —
  the stage flow and the `ResolvedSpec`/fingerprint edge (owner).
- [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#primary-runtime-axes)
  — the primary/supporting axis catalog (owner).
- [tool-and-capability.md](../design/tool-and-capability.md#capability-segments) —
  the capability-segment overlay dimensions.
- [ADR-0002](0002-resolver-role-demarcation.md) — the resolver roles this closes.
- [INVARIANTS.md](../INVARIANTS.md) — G3, G4, G9, G18, G21.
