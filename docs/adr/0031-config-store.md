# ADR-0031: Config Store — Compilation and Durable Publication

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0029, config-publication-lifecycle.md
- Amended by: ADR-0071, which supersedes the whole-catalog handoff in D1/D4
  with per-Agent executable registration while retaining compilation and durable
  publication ownership

## Context

The runtime *consumes* an `ExecutableAgentSnapshot` and validates it against the
installed catalog fingerprint (G4/G28), but nothing *produced* one — tests built
snapshots by hand. The config domain owns authoring, compilation, and publication;
the runtime owns only validation and install
(`config-publication-lifecycle.md`). This builds the config side: compile a
declarative agent config into a published, content-addressed catalog the runtime
installs, stored durably under its own namespace.

## Decision

### D1: Two bounded contexts; a published artifact is the only seam

Config domain and runtime core are separate contexts. Their integration is an
immutable **published language**: the contract types `ExecutableAgentSnapshot` and
`RuntimeCatalogInstall`. The config store produces them; the runtime consumes them
as opaque resolved data. Neither edits the other's truth — the config store never
executes, the runtime never edits config records (DDD context mapping).

### D2: Compilation is a pure, content-addressed function

`compile(config, tools)` is a pure domain function (no infrastructure). It
resolves each `tool_id` against the available tool catalog — an unknown reference
is rejected before publication (fail-closed, the design's Failure Rules) — and
emits a `Publication { fingerprint, snapshot, install }`. The fingerprint is
`sha256` of the canonical config serialization, so a publication is **content
addressed**: the same config always yields the same fingerprint, and the snapshot,
the resolved spec, and the install all carry it — exactly the identity the
runtime's `resolve` re-validates (defense in depth).

### D3: Durable storage under the `config` namespace

The store persists agent configs and publications. It is a different *component*
from the runtime, so it uses a built-in `config` table namespace (`config_agent`,
`config_publication`) with its own migration ledger (`config_schema_migrations`),
coexisting with the runtime's `runtime_*` tables in one database (ADR-0029). The
`ConfigStore` port has SQLite and Postgres adapters, mirroring the commit and
dispatch stores; the SQL is the same portable scoped-migration bundle.

### D4: The lifecycle spine, not the full state machine

This builds `draft → compiled → published` and `install_candidate()` (the
`RuntimeCatalogInstall` handoff). The full lifecycle
(`installing/active/superseded/rolled_back/rejected`, registry-graph validation,
change notification, publication audit) is named in
`config-publication-lifecycle.md` and deferred — the spine is what closes the
produce-a-snapshot gap.

## Consequences

- The config domain produces what the runtime consumes: `config → compile →
  publish → install → execute` is closed end to end and tested.
- Publications are content-addressed; install is idempotent by fingerprint.
- Config and runtime tables coexist in one database, isolated by namespace and
  ledger (ADR-0029), with no shared schema.
- The richer lifecycle, graph validation, and audit remain deferred, named here.

## Amendment (2026-06-30, ADR-0032)

The seam (D1) and the `compile` output (D2) are now bundled into one value object,
`RunnableConfig`, which pairs the snapshot and the install under one fingerprint —
the runtime's single run input, buildable directly or by `compile`. The in-memory
`Publication` struct is removed (subsumed by `RunnableConfig`); `compile` returns a
`RunnableConfig`. The durable `StoredPublication` and the publication lifecycle are
unchanged. See [ADR-0032](0032-runnable-config.md).

## References

- [INVARIANTS.md](../INVARIANTS.md) — G4, G22, G28 (resolution fail-closed).
- [config-publication-lifecycle.md](../design/config-publication-lifecycle.md).
- ADR-0029 — the built-in component namespace this reuses for `config`.

## Amendment (2026-07-30, ADR-0071)

The durable config and publication decisions remain authoritative. The former
whole-catalog handoff is historical. The current implementation persists
`StoredPublication` and updates a process-local catalog; the accepted target
replaces that second step with `ExecutableAgentRegistrar::register` and a
rebuildable Coordinator projection. See
[ADR-0071](0071-distributed-service-boundaries-and-executable-agent-registration.md).

## Amendment (2026-08-29): executable-only fingerprints and one compaction trigger

`AgentConfig` remains the lossless authoring aggregate, while
`ExecutableAgentSnapshot` remains the only Runtime publication. Compilation now
canonicalizes `plugin_config` at that boundary: a selected plugin section is
executable; the backend-owned `acp` section is executable only for an ACP route;
unselected residue remains editable but is excluded from both the snapshot and
its fingerprint. The existing legacy `permission` codec remains executable until
its already-decided migration into typed Agent bindings; it is not classified as
an inactive plugin.

`AgentConfig.compaction` is the sole authored trigger strategy. Publication
derives the effective token window once from the pinned model context/output
limits, creates the selected Native compact or ACP section when needed, and
stamps that same effective value into both realizations. Legacy JSON
`max_tokens`, `trigger_ratio`, `threshold`, `keep_last`, and `compact_window`
cannot override the typed decision: the sole effective `max_tokens` window is
replaced or removed, legacy count/ratio trigger fields are absent, and
`keep_last` is present only when `keep_recent` is authored. Runtime realizers
decode the frozen result and make no second authoring decision; no effective
window means no compaction rather than a message-count fallback.

Static ownership is unchanged: Config owns intent and compilation, the plugin
registry owns activation, and Runtime owns execution. No compatibility flag,
dual write, synchronized fingerprint, or second compaction state machine is
introduced.

| Rule | Selected/executable? | Config change | Publication effect |
|---|---|---|---|
| F1 | no | inactive plugin residue changes | identical snapshot and fingerprint |
| F2 | yes | active plugin config changes | changed executable fingerprint |
| F3 | Native | ACP-only residue changes | identical Native snapshot and fingerprint |
| F4 | ACP | ACP section changes | changed executable fingerprint |
| T1 | compact or ACP realization selected | legacy trigger conflicts with typed strategy | typed derived trigger wins; legacy threshold is absent |
