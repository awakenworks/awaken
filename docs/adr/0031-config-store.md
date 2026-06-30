# ADR-0031: Config Store — Compilation and Durable Publication

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0029, config-publication-lifecycle.md

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
