# ADR-0082: Open Specification, Closed Substrate

- Status: Proposed
- Deferred: 2026-09-20 — the product owner chose to keep ADR-0041's placement for now;
  revisit when the closed backends need a second substrate or when a self-hosted
  multi-node floor is decided for the open product.
- Date: 2026-09-20
- Builds on: ADR-0081 contract layering (interface kinds, capability ladders, guard-passing
  fences, ratchet guardrails, and the `awaken-store-conformance` suites every SPI backend
  runs); ADR-0059 neutral core and leaf evolution.
- Would supersede in part: ADR-0008, ADR-0012 and ADR-0028 (the open-source product ships
  durable multi-node backends), and the row of the Cloud repository's ADR-0041 that places
  Kubernetes/Docker/Podman providers and warm capacity in the open Worker.

## Context

The product direction is that the open-source repository keeps the basic, single-node
product and that most stability, reliability and distributed capability lives in the
closed `awaken-cloud` repository. Today the split is the reverse: 28 open crates carry a
PostgreSQL backend, 9 carry Kubernetes, and the closed repository implements none of the
open durable ports (`WorkQueue`, `DispatchQueue`, `SessionRepository`,
`ResourceRegistryRepository`, `EnvRegistry`, `WorkerDirectory`). Its own manifest says so:
"the open runtime owns dispatch, commit fencing and worker transports; this crate supplies
the production database choice". ADR-0081 §9 declared the move out of scope because it
contradicts ADR-0008/0012/0028 here and ADR-0001/0040/0041 in the Cloud repository, and
asked for a product decision with its own ADR. This is that ADR.

The Cloud repository's ADR-0041 retired a closed fleet registry and scheduler because two
placement authorities and two sandbox lifecycle models would drift apart. That objection
is real and this decision must answer it, not repeat the failure it corrected.

## Decision

Reliability is not a body of code that can be moved; it is the set of invariants the
aggregates hold (epoch monotonicity, one CAS winner, idempotent replay, terminal states
that never reopen). Removing those from the open product would not make it "basic", it
would make it fail open. So the line is drawn between **specification** and
**substrate**, the way Kubernetes/cloud providers, Temporal/Temporal Cloud and
Postgres/Aurora draw it:

1. **Open: specification and single-node correctness.** Every contract crate (types,
   spi, api, remote, ext per ADR-0081 §1), every aggregate and application service —
   including the realization-lease laws, the fence succession kernel, terminal cleanup,
   dispatch fencing and the `SessionRealizationControl` implementation — the
   `awaken-store-conformance` suites for every SPI, the in-memory and SQLite backends,
   the local and namespace sandbox providers, one Worker, and one Coordinator.
2. **Closed: substrate, scale and operations.** The PostgreSQL implementations of every
   SPI (each store crate's `postgres*.rs`, split into a `*-postgres` crate per port),
   the Kubernetes and container-pool providers implementing
   `RestorableContainerEnvironmentProvider`, warm capacity, checkpoint object stores,
   multi-Worker recovery scanning, reclamation at scale, migrations, region federation,
   the egress gateway, billing and the console.
3. **One authority, many substrates.** ADR-0041's "one Worker/placement/sandbox lifecycle
   authority" stays true: the authority is the open aggregate plus the conformance
   laws, and every closed substrate must pass the same suite a SQLite backend passes.
   A closed backend that diverges fails the open suite; it cannot become a second
   authority. The Cloud ADR-0041 row changes to: "Open owns the provider SPI, the
   conformance suite and the local/namespace providers; cloud-substrate provider
   implementations are closed."
4. **Order of work.** The contract layer must be able to carry the split before anything
   moves: opaque sandbox handles, the capability ladders, guard-passing fences and the
   per-SPI conformance suites are prerequisites (delivered under ADR-0081 and this
   change set). Only then does each PostgreSQL implementation move, one port at a time,
   with the open CI still running that port's suite on SQLite and the closed CI running
   it on PostgreSQL.

## Consequences

- The open product remains a complete, correct, self-hostable single-node runtime with
  SQLite and local sandboxes. Self-hosted multi-node deployment on PostgreSQL and
  Kubernetes becomes a closed capability; ADR-0001 rule 4 in the Cloud repository must
  be re-read with that narrower floor and the product owner must confirm it.
- ADR-0008, ADR-0012 and ADR-0028 are superseded in the part that promises PostgreSQL
  backends in this repository; the store ports and their SQLite backends stay.
- Every SPI gains an external implementor, so contract crates are versioned and
  consumed by semver from the closed repository instead of a pinned git revision.
- Not decided here: which crate boundary each PostgreSQL backend gets, and the
  timeline. Each move is its own change with the conformance suite as its acceptance
  test.
