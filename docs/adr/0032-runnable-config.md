# ADR-0032: RunnableConfig — The Runtime's Directly-Buildable Run Input

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0031, ADR-0029

## Context

ADR-0031 made the runtime's input two loose contract types —
`ExecutableAgentSnapshot` and `RuntimeCatalogInstall` — produced only by the config
domain's `compile()`. Two problems followed for anyone running the runtime
directly (the embedded, "bring your own producer" case):

- Hand-building the pair meant assembling a snapshot, an install, and writing the
  same fingerprint into four places by hand — a footgun the runtime's fail-closed
  resolution exists to catch, but which is easy to get wrong.
- The runtime could not be used without the config store: there was no ergonomic
  way to produce a runnable thing except `compile()`.

The goal: make the runtime self-sufficient and directly configurable by hand,
while keeping `compile()` as one *external, optional* producer and never letting
the runtime compute fingerprints (the neutral waist, G3/G4).

## Decision

### D1: One seam bundle, parts kept consistent

`RunnableConfig` (in `awaken-runtime-contract`) bundles the snapshot and the
install under one fingerprint and is the runtime's single run input. The two parts
are private — the only ways to make a `RunnableConfig` stamp a consistent
fingerprint into both, so they cannot drift apart.

### D2: One assembly path, two producers

`RunnableConfig::builder` is the canonical assembly: `instructions` / `model` /
`tools` / `max_steps` become the snapshot and install, with the fingerprint
stamped once. `compile()` becomes a thin wrapper — resolve tool ids to
descriptors, hash the config, then call the same builder with
`.fingerprint(sha256)`. The direct path and the compiled path share one assembly,
so there is no duplicated snapshot/install construction (`Publication` is removed,
subsumed by `RunnableConfig`).

### D3: The fingerprint is stamped, never hand-written, never computed by the runtime

A direct build stamps the agent id as the consistency token (enough for in-process
use, where content-addressing is not needed); a compiler stamps the content hash.
Either way the runtime only *checks* that the snapshot and the installed catalog
agree (fail-closed, G28) — it never hashes, so the config-domain knowledge stays
out of the runtime.

### D4: One-call `run`, gate intact

`Runtime::run(&config, input, ctx)` installs the config's catalog (idempotent) and
executes one fresh turn — the embedded ergonomic entry. The fingerprint gate still
holds: the snapshot is resolved against the catalog just installed, so a forged or
mismatched config still fails closed. The low-level `install_catalog` + `execute`
remain for the durable/distributed path, where the catalog is installed once and
many runs follow.

## Consequences

- The runtime is self-sufficient: `RunnableConfig::builder` needs no config store;
  `compile()` is one optional external producer.
- `direct_runtime` and `hello_agent` converge on `runtime.run`, differing only in
  how the `RunnableConfig` is built (builder vs `compile`).
- The seam is one type, not two loose ones, and the fingerprint-four-times footgun
  is gone — assembly stamps it once.
- The durable path is unchanged; `run` is additive sugar over `install_catalog` +
  `execute`.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G3/G4 (neutral waist), G28 (resolution fail-closed).
- ADR-0031 — the config store whose `compile()` now returns a `RunnableConfig`.
- ADR-0029 — the built-in namespace the config store stores under.
