# ADR-0004: Plugin Factory, Contributions, And Capability Bound

- Status: Accepted
- Depends on: ADR-0001
- Supersedes: the mutable `PluginRegistrar` registration seam and the separate
  `PluginConfigValidator` port (both collapse into the model below)

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
cannot read a section it never declared.

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
decoding targets the same type, so schema and decode cannot disagree. The
separate `PluginConfigValidator` port is removed.

### D4: `CapabilityBound` is a declared, fail-closed upper bound

The manifest declares what a plugin **may** contribute. This is a ceiling, not an
inventory and not an authorization: the only authoritative list of what a plugin
contributes is its `Contributions`, and the only authorization path remains the
permission policy (G9, G21). "Bound", not "grant", is used deliberately so the
word `grant` stays reserved for authorization (ADR-0001 D3).

- Capabilities with addressable identity declare a set or namespace: tools,
  state keys, guards.
- Singleton powers with no instance identity are flags: the power to intercept
  any tool call, and the power to rewrite the whole inference request.

After `resolve`, `enforce_bound(manifest, contributions)` checks
`actual ⊆ declared` and fails the resolution closed on any excess; the catalog
runs the same check at registration so a plugin whose default behavior already
exceeds its bound fails at boot, not at first use. This is the fail-closed shape
of G4/G5 applied to the contribution boundary, and the plugin-scoped analogue of
the pinned tool-descriptor segment (D6, G8). The concrete inventory for operator
UIs is derived by a dry-run `resolve`, never hand-maintained.

### D5: Contribution identity is fixed by construction

A tool id is one literal: the registration key, the `ToolDescriptor` id, and the
`CapabilityBound` reference are the same value by construction, not by
convention. A dynamic tool family declares a single namespace that feeds both the
bound and the id constructor, and `enforce_bound`'s prefix check is the backstop.

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

## References

- INVARIANTS G8 (capability segmentation), G9/G21 (permission is the only grant
  path), G14 (development-ready rule), G30 (contribution bound).
- Key design decisions D11 (state/effects/extension seams), D21 (activation data
  versus runtime context).
