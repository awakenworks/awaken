# ADR-0039: Runtime Persistence — Port Convergence and the `Store` Naming Rule

- Status: Accepted
- Date: 2026-07-02
- Builds on: [ADR-0006](0006-fact-authority-run-record-is-cache.md) (facts are the
  authority; the run record is a cache), [ADR-0009](0009-durable-run-ingress-slice.md)
  (durable ingress slice), [ADR-0034](0034-runtime-axis-model-and-orthogonality.md)
  (axis model), [ADR-0038](0038-managed-resource-injection-and-store-organization.md)
  (three resource aggregates, one descriptor spine)
- Relates to: G1, G2, G5, G6, G13, G16, G18
- Supersedes: the **port names** in ADR-0038 D1/D3 (`FileStore` → `File`,
  `SkillStore` → `SkillRegistry`; `MemoryStore` kept). The aggregate structure and
  co-location rule of ADR-0038 are unchanged.

## Context

Phase 2 deepens the runtime-truth persistence layer (commit, committed facts and
events, dispatch, config) toward a full single-machine implementation. A survey of
the reference implementation showed a large port surface — separate writer / reader
/ lookup / subscriber traits per concern, a six-trait dispatch family, per-consumer
cursor stores, distributed wake-hint and live-command stores, and an umbrella store
facade. That surface is driven by three axes the single-machine target does not
have: multi-backend interface segregation, distribution, and pluggable role-scoped
assembly. Importing it wholesale would add elements no implemented slice needs,
violating simple design rule 4 (fewest elements: a port earns its place only if it
owns a distinct authority, failure mode, and guardrail).

Two orthogonal problems are settled here: **how many persistence ports P2 needs**,
and **what they are named** — the word `Store` is currently used both as a generic
persistence suffix and as a domain aggregate (`MemoryStore`), and the proposed
backend crate `awaken-store-memory` reads as the resource concept "memory store".

## Decision

### D1: One write boundary and one read repository per aggregate

Persistence ports are scoped by aggregate, not by verb or by backend. The runtime
truth aggregate (thread transcript, run, state, committed events) has exactly:

- **`CommitCoordinator`** — the single durable write boundary (unchanged; G1).
  `commit(ThreadCommit) -> CommitRecord`.
- **`CheckpointReader`** — the single after-commit read repository. It subsumes
  today's `ThreadReader` and `RunStore`: `committed_messages`, `committed_state`,
  `run`, `latest_run`, `resume_ticket`, and `list_events(scope, cursor)`.
- **`StreamSink`** — best-effort live delivery (unchanged; G10).

The dispatch aggregate keeps its existing cohesive composite —
`DispatchQueue` + `Inbox` + `Outbox` — and is **not** split into queue / lifecycle
/ query / maintenance / wake-hint / live-command role traits. The config aggregate
has one port, `ConfigRegistry`.

This rejects the per-verb / per-backend trait explosion: a reader that also needs
lookup and subscribe is one repository, not three ports, because single-machine has
one client and one backend implementing all of it. Interface segregation is a means
used where a real client needs a subset, not a goal.

### D2: `Store` names only a mutable keyed store; every other port names its authority

> **Amended 2026-07-04 — the `Store`-suffix *ban* is dropped.** `Store` is a
> generic, well-understood persistence word; forbidding it as a type suffix was
> false precision and blocked natural names (e.g. `SecretStore`). D2 is retained
> only as a **preference**: when a port has a sharper role word (commit, queue,
> checkpoint, delivery), prefer that word because it reveals intent — but a plain
> `*Store` for a genuine keyed/persistence port is allowed and is not a defect.
> The specific rename decisions D2 drove (`CommitCoordinator`, `CheckpointReader`,
> `DispatchQueue`, `Inbox`/`Outbox`, `ConfigRegistry`) stand as implemented; this
> amendment removes only the blanket prohibition, not those names.

`Store` is a container word with no intent. It is reserved for the one shape that
genuinely *is* a store — a mutable, keyed put/get collection. Under this rule:

- keep `MemoryStore` (a mutable read-write named store) and `ConfigRegistry`
  semantics that are keyed put/get;
- everything else names the authority it holds: `CommitCoordinator` (commit),
  `CheckpointReader` (read committed truth), `EventLog` (append-only history),
  `DispatchQueue` (queue), `Worker` (claim/run loop), `Signal` (wake), `Inbox` /
  `Outbox` (delivery).

No type or trait carries a bare `Store` suffix except `MemoryStore`. A uniform
`*Store` suffix across differently-shaped things is false consistency; it hides the
lifecycle differences ADR-0038 D1 deliberately kept apart. The rename list is in
the Implementation section.

### D3: Durable events and outbox intents commit inside the checkpoint transaction

The current `EventLog::append` is a synchronous side-write, separate from the
commit. It is removed as a public path. Durable events and cross-thread outbox
intents are carried on `ThreadCommit` and made visible atomically with the
checkpoint, or they are derived after commit — never as a parallel writer (G13).
`CheckpointReader::list_events` is the read side. Live streaming stays best-effort
through `StreamSink` and is reconciled by committed history.

### D4: Reads derive from committed facts, not an in-process projection cache

Today `PostgresCommitCoordinator` / `SqliteCommitCoordinator` serve `ThreadReader`
and `RunStore` from an in-memory `Mutex<Projection>`. That projection is empty in a
fresh process, so a durable run cannot resume after a restart — the exact failure
the durable slice exists to prevent (ADR-0009). `CheckpointReader` must read from
the committed tables / fact log (ADR-0006: facts are the authority, the run record
is a cache). A `FactCheckpointReader` that rebuilds the read model from the fact
log is a backend implementation detail, not a port.

### D5: The persistence seam keeps the `awaken-store-<medium>` category; only the colliding media are renamed

`store` is this repo's category prefix for the persistence seam, parallel to
`provider` / `protocol` / `sandbox` / `ext`. At the *category* layer it names a
seam, not a filler suffix, so it stays. The collision was never in `store` — it was
in pairing it with a medium word that is also a resource aggregate
(`store-memory` ↔ `MemoryStore`, `store-file` ↔ `File`). The fix is to name the two
offending media precisely instead of colloquially:

| Now / new | Name |
|---|---|
| `awaken-store-postgres` | unchanged |
| `awaken-store-sqlite` | unchanged |
| `awaken-store-schema` | unchanged |
| RAM backend (new) | `awaken-store-inmem` |
| filesystem backend (new) | `awaken-store-fs` |

`inmem` and `fs` are unambiguous media, so neither collides with the `MemoryStore`
or `File` aggregates, and the existing postgres/sqlite/schema crates are untouched.
This does not weaken D2: no *type* carries a bare `Store` suffix except
`MemoryStore`; `store` as a *crate category* is a different layer and syntactic
position (package prefix, not type suffix).

### D6: Single-machine declines the distribution-driven ports until a slice needs them

Deferred, consistent with ADR-0038 D6 (borrow the spine, decline the machinery):
per-consumer cursor stores, durable wake-hint and live-command stores (single
machine uses in-process `Signal`/broadcast), the protocol-replay log (P3, an
adapter projection per the commit/fact/projection taxonomy), the versioned registry
(P1 config data plane), and NATS (ADR-0028). Each returns as a real port when a
concrete slice requires it, not before.

## Implementation

### Rename list

`ThreadReader` + `RunStore` → **`CheckpointReader`**; `MessageOutbox` → **`Outbox`**;
`PendingInbox` → **`Inbox`**; `RunDispatch` / `DispatchStore` → **`DispatchQueue`**
(composite `Dispatch`); wake hint → **`Signal`**; `ConfigStore` → **`ConfigRegistry`**;
new backend crates are **`awaken-store-inmem`** / **`awaken-store-fs`** (existing
postgres/sqlite/schema unchanged, D5); (ADR-0038) `FileStore`
→ **`File`** (port `Files`), `SkillStore` → **`SkillRegistry`**, `MemoryStore` kept.
`ThreadCommit` and `CommitCoordinator` are unchanged.

### Ports, owners, guardrails (G14)

| Aggregate | Ports | Owning contract crate | Guardrail |
|---|---|---|---|
| Runtime truth | `CommitCoordinator`, `CheckpointReader`, `StreamSink` | `awaken-agent-contract` | G1, G13 |
| Dispatch | `DispatchQueue`, `Inbox`, `Outbox` | new `awaken-run-ingress-contract` | G5, G6, G18 |
| Config | `ConfigRegistry` | `awaken-config-store` | G3 |

Backends (`awaken-store-*`) implement the union; the direction is enforced by
`deny.toml` (backends depend on contracts, never the reverse — G2) and a
cross-backend conformance suite.

### Slices

- **2.1** — extract `awaken-run-ingress-contract` (move + rename `DispatchQueue` /
  `Inbox` / `Outbox`); define `CheckpointReader` and extend `ThreadCommit` with
  events + outbox intents in `awaken-agent-contract`; apply the D2 rename set.
  Behavior-preserving.
- **2.2 (first vertical slice)** — `awaken-store-inmem` implements
  `CommitCoordinator` + `CheckpointReader`. Test: commit a checkpoint, drop the
  in-process projection, and resume through a fresh reader instance (proves D4).
- **2.3** — `awaken-store-fs`: temp-file + rename + checkpoint journal + crash
  recovery on open; scope-indexed append-only event log.
- **2.4** — `awaken-store-postgres` full surface; reads from committed tables (D4);
  staged writes in one SQL transaction (D3).
- **2.5** — `awaken-store-sqlite` parity.
- **2.6** — one trait-generic conformance suite every backend runs (commit
  atomicity, event order G1, cursor paging, idempotency, crash recovery, cross-instance
  resume, and a G13 projection-failure-does-not-uncommit case); durable dispatch backend.

## Consequences

- The P2 persistence SPI is ~5 ports, not ~30; each names one authority and has a
  conformance test, satisfying simple design rules 2 and 4.
- `Store` has one meaning at the type layer (`MemoryStore`) and one at the crate
  layer (the persistence seam category); the `store-memory` / `MemoryStore` collision
  is gone because the media are named `inmem` / `fs`.
- Durable reads survive process restart (D4), closing a real resume gap in the
  current SQL backends.
- Distribution stays enabled but unbuilt (D6); adding a deferred port later does not
  reshape the existing ones.

## Alternatives considered

- **Import the reference port surface** (writer/reader/lookup/subscriber splits,
  six-trait dispatch, cursor/wake-hint/live-command stores, store facade). Rejected
  (D1/D6): those elements serve multi-backend ISP, distribution, and pluggable
  assembly, none present single-machine.
- **Keep a uniform `*Store` suffix** (`FileStore`/`MemoryStore`/`SkillStore`,
  `DispatchStore`, `ConfigStore`). Rejected (D2): false consistency that hides real
  lifecycle differences and overloads `Store`.
- **Rename the persistence crates off `store` (e.g. to `persist`).** Rejected (D5):
  `store` is the seam category (parallel to `provider`/`protocol`/`sandbox`), not the
  collision; `persist` is a vaguer verb-noun (and `persist-memory` reads as
  persistent-memory hardware). Naming only the two overloaded media precisely
  (`inmem`/`fs`) removes the collision with no churn to existing crates.
- **Let events/outbox stay side-writes.** Rejected (D3): violates G13 single-source
  commit.

## References

- [ADR-0006](0006-fact-authority-run-record-is-cache.md),
  [ADR-0009](0009-durable-run-ingress-slice.md),
  [ADR-0038](0038-managed-resource-injection-and-store-organization.md).
- [INVARIANTS.md](../INVARIANTS.md) — G1, G2, G5, G6, G13, G18.
- [commit-fact-projection-taxonomy.md](../design/commit-fact-projection-taxonomy.md).
