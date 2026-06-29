# ADR-0004: Plugin Factory, Contributions, And Capability Bound

- Status: Accepted
- Amended: 2026-06-29 — D2/D3/D4/D5 refined against the awaken reference
  implementation; see Amendment A1
- Depends on: ADR-0001
- Supersedes: the mutable `PluginRegistrar` registration seam; the config-blind
  `register_runtime(&mut PluginRegistrar)` seam (replaced by `resolve`, not
  retained). The separate `PluginConfigValidator` *implementation* collapses into
  one `validate_section` home (D3); the trait survives only as the thin
  runtime↔server DI seam that calls it (Amendment A1).

## Context

The plugin extension seam contributes behavior through a mutable registration
step and validates plugin config through a port that is separate from the seam
that consumes it. Three problems follow, and all are maintained by discipline
rather than by mechanism:

- **The aggregate never sees all contributions at once.** A mutable
  registration step is a side effect: each plugin pushes into a shared registrar,
  so no single value holds every plugin's contributions together. Cross-plugin
  ordering and uniform conflict detection are therefore impossible to enforce —
  duplicate ids and incidental ordering pass silently.
- **Config is validated in two places.** A config-time validator and the
  resolution path each parse the same plugin config against the same schema; the
  two can drift, and a config-dependent artifact is recompiled on every hook call
  because registration cannot see the config.
- **A plugin may contribute anything.** There is no declared upper bound on what
  a plugin contributes, so an added tool or an acquired interception power is
  invisible until it runs, and an operator cannot reason about a plugin before a
  run.

## Decision

### D1: A plugin is a factory; the execution environment is the aggregate root

A plugin exposes a manifest and a single factory method:

```text
trait Plugin {
    fn manifest(&self) -> &PluginManifest;
    fn resolve(&self, cx: &ResolveContext) -> Result<Contributions, ResolveError>;
}
```

`resolve` returns `Contributions` — an immutable value object of one plugin's
tools, hooks, gates, guards, transforms, and state keys. `ResolvedExecutionEnv`
is the aggregate root: it merges every selected plugin's `Contributions` and owns
their invariants. The factory returns; the aggregate root assembles. The mutable
registration seam is retired.

### D2: `resolve` is config-aware — compile once, fail at resolve

`ResolveContext` carries the resolved `AgentSpec`, so a config-dependent artifact
is compiled exactly once inside `resolve` and the produced hooks close over it.
Config errors surface at resolve (fail closed), not at the Nth call.
`ResolveContext` exposes only the config sections the manifest declares; a plugin
cannot read a section it never declared. To make this a mechanism and not a
convention, the context exposes **no raw `AgentSpec` accessor at all** — typed
config is reachable only through the confined `config::<K>()`, so there is no
`agent_spec().config()` bypass. A future need for non-config spec data is served
by a narrow typed accessor, never by re-exposing the whole spec (Amendment A1).

### D3: `PluginManifest` is the single identity-and-config contract

`PluginManifest` is pure data and lives in `awaken-agent-contract`, so config and
admin paths can read it without a runtime dependency:

```text
PluginManifest {
    id,                 // owned id; config-driven plugins allowed
    requires,           // declared plugin dependencies -> topological order
    config,             // declared config sections
    bound,              // CapabilityBound (D4)
}
```

Config validation has one home, `validate_section`, used by both write-time
(config/admin) and resolve-time. The schema is derived from the typed config and
decoding targets the same type, so schema and decode cannot disagree. There is no
second validation *implementation*; the `PluginConfigValidator` trait is retained
only as the thin runtime↔server DI seam that forwards to `validate_section` (the
server depends on the contract trait, not on the concrete runtime). Collapsing the
duplicate logic — not deleting the seam — is the win (Amendment A1).

### D4: `CapabilityBound` is a declared, fail-closed upper bound

The manifest declares what a plugin **may** contribute. This is a ceiling, not an
inventory and not an authorization: the only authoritative list of what a plugin
contributes is its `Contributions`, and the only authorization path remains the
permission policy (G9, G21). "Bound", not "grant", is used deliberately so the
word `grant` stays reserved for authorization (ADR-0001 D3). This separation is
**type-level, not just naming**: no type is named `Grant`, and there is no
conversion between `CapabilityBound`/`IdBound` and any permission-decision type,
so a structural ceiling and an authorization decision cannot be confused at the
type level (Amendment A1).

- Capabilities with addressable identity declare a set or namespace. **Every**
  identity-bearing contribution kind is bounded: `tools`, `state_keys`, `guards`,
  `effects`, and `scheduled_actions` — effects are the state-mutation carrier and
  scheduled actions are deferred mutations, so they belong under the ceiling too
  (Amendment A1).
- Singleton powers with no instance identity are flags: `tool_gate` (the power to
  intercept any tool call), and `transforms` (the power to rewrite the whole
  inference request).

Each id-bearing dimension is an `IdBound`: `Any | Exact(ids) | Namespace(prefix)
| NamespacedExact { prefix, ids }`. `NamespacedExact` is the finer ceiling for a
dynamically discovered family (MCP/skills): an id is admitted only if it is both
under the prefix **and** in the explicit list, so a stray prefixed id that did not
come from discovery — a hardcoded backdoor — is rejected, which a bare `Namespace`
prefix would admit (Amendment A1, supersedes the coarse prefix-only treatment in
D5).

After `resolve`, `enforce_bound(manifest, contributions)` checks
`actual ⊆ declared` and fails the resolution closed on any excess; the catalog
runs the same check at registration so a plugin whose default behavior already
exceeds its bound fails at boot, not at first use. This is the fail-closed shape
of G4/G5 applied to the contribution boundary, and the plugin-scoped analogue of
the pinned tool-descriptor segment (D6, G8). The concrete inventory for operator
UIs is derived by a dry-run `resolve`, never hand-maintained.

**Resolve-time tightened sub-bound (dynamic families).** A dynamic plugin cannot
enumerate its ids in a static manifest, yet a coarse `Namespace` ceiling is weak.
It therefore declares the coarse `Namespace` ceiling statically and, inside
`resolve`, submits a tightened sub-bound (`Contributions::tighten_bound`) built
from the **same discovery snapshot** that produced its contributions — so the
bound and the contributions cannot drift even if the underlying registry refreshes
concurrently (a real time-of-check hazard, since the source registry is live).
`enforce_bound` then runs two checks: the tightened bound must be `within` the
static manifest ceiling (`CapabilityBound::within` / `IdBound::within` — a
tightening can only narrow, never escape the declared namespace or claim an
ungranted `tool_gate`/`transforms`), and the contributions must stay within the
tightened exact set. The tightened bound doubles as the derived operator inventory
(Amendment A1).

### D5: Contribution identity is fixed by construction

A tool id is one literal: the registration key, the `ToolDescriptor` id, and the
`CapabilityBound` reference are the same value by construction, not by
convention. A dynamic tool family declares a single namespace constant that feeds
both the bound and the id constructor; the coarse prefix is the static ceiling,
and the resolve-time tightened sub-bound (D4) narrows it to the exact discovered
ids — the prefix check is the backstop, the tightened set is the real bound.

### D6: Uniqueness and ordering belong to the aggregate root

`ResolvedExecutionEnv` merge replaces ad-hoc per-kind dedup with one policy:

- Uniqueness is scoped to the resolved agent's active plugin set, not the
  catalog: two plugins may each define a `search` tool in the catalog; the clash
  exists only when they are co-activated and fails closed there, naming both
  plugin ids (the agent dispatches by name, so the name must be unambiguous within
  one resolved env).
- All contribution kinds are conflict-checked, including hooks, gates, guards,
  and transforms.
- Order is deterministic and declared: across plugins by `requires` topological
  order (missing dependency or cycle fails closed), within a plugin by an explicit
  order on the contribution — never an incidental plugin-id list.
- Persisted state and profile keys must fall within the plugin's declared
  namespace (a shared key is state corruption, G13); agent-facing first-party tool
  names may be unprefixed because collisions are rare and surface early.

### D7: Run handles flow through the call context

Per-run handles (store, runtime input, cancellation) reach hooks through the call
context at the call site; they are not bound into the plugin. `ResolvedExecutionEnv`
is therefore immutable and safely pooled across runs, consistent with the
activation-versus-context split (D21). A long-lived runtime service is reached as
an injected port through the call context, never via interior mutability.

## Consequences

- A plugin author writes a contract (`manifest`) and a factory (`resolve`);
  config-dependent artifacts compile once.
- Governance is possible before a run: operator policy can allow or deny a plugin
  by its declared `CapabilityBound` — especially the sensitive interception and
  request-rewrite powers — without resolving it.
- The extension surface gains a real type boundary: config paths touch only
  `PluginManifest` (no runtime handles), execution paths touch only
  `Plugin` / `Contributions`.
- `ResolvedExecutionEnv` immutability keeps cross-run pooling safe.
- The neutral kernel stays domain-agnostic: `Contributions` is the
  anti-corruption value between an extension and the kernel; the kernel
  enumerates no plugin vocabulary.

## Amendment A1 (2026-06-29): refinements proven in implementation

These refine D2–D5 against the awaken runtime implementation, which carried this
design to a working, tested state and surfaced what the original text under- or
mis-specified. They are corrections and additions, not a new direction.

1. **Bound vs grant is type-level (D4).** Reserving the *word* `grant` is not
   enough; the separation must be unforgeable. No type is named `Grant`, and
   `CapabilityBound`/`IdBound` have no conversion to any permission-decision type,
   so passing `enforce_bound` can never be mistaken for "authorized".
2. **Every id-bearing kind is bounded (D4).** `effects` and `scheduled_actions`
   join `tools`/`state_keys`/`guards` under the ceiling — state mutation and
   deferred mutation are identity-bearing and must not be unbounded.
3. **`IdBound::NamespacedExact { prefix, ids }` (D4/D5).** A prefix-plus-explicit
   list ceiling for dynamic families, strictly finer than a bare `Namespace`.
4. **Resolve-time tightened sub-bound + `within` (D4).** A dynamic plugin
   declares the coarse `Namespace` statically and submits a tightened sub-bound
   from the same discovery snapshot; `enforce_bound` checks
   `contributions ⊆ tightened ⊆ static ceiling`. This closes a real
   time-of-check/time-of-use gap (the source registry is live) that a single
   static or single re-queried bound would leave open.
5. **Config confinement is mechanized, not conventional (D2).** `ResolveContext`
   exposes no raw `AgentSpec` accessor at all; the confined `config::<K>()` is the
   only config path, so `agent_spec().config()` cannot bypass confinement.
6. **`PluginConfigValidator` is reconciled (D3, Supersedes).** The duplicate
   validation *logic* collapses into `validate_section`; the trait is kept as the
   thin runtime↔server DI seam. Deleting the seam (as the original text implied)
   would remove a real abstraction, not a duplication.

### Still open — required for the governance payoff (G8 operator overlay)

The bound currently delivers drift-prevention (self-check `actual ⊆ declared`) but
not yet operator governance. Two additions close that, and neither is yet built in
the reference implementation:

- **`CapabilityBound` must derive `Serialize`/`Deserialize`.** It is pure data in
  the contract crate; to preview and allow/deny by bound on the config/admin
  plane it has to cross that boundary as data.
- **`PluginCapability` (capability.rs) must carry a bound projection.** Extend
  `RuntimeCapabilityCatalog`'s `PluginCapability { id, schema_keys }` with the
  declared `bound` (derived by dry-run `resolve`, per D4), so an operator overlay
  can deny a plugin by its declared `tool_gate` / `transforms` / namespace before
  a run — the consumer side of G8/G30 that the catalog surface does not yet expose.

## References

- INVARIANTS G8 (capability segmentation), G9/G21 (permission is the only grant
  path), G14 (development-ready rule), G30 (contribution bound).
- Key design decisions D11 (state/effects/extension seams), D21 (activation data
  versus runtime context).
