# ADR-0029: A Built-In Runtime Table Namespace, Not a Configured Prefix

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0008, ADR-0012

## Context

The Postgres and SQLite stores (dispatch and commit) took a `prefix: impl
Into<String>` so several components could share one database via scoped
migrations. In practice only the test suite ever passed *different* prefixes — as
a parallel-isolation token — and no production caller existed. The namespacing the
prefix really provides is by *component* (the runtime's tables vs another
component's), not by *service/tenant* (two deployments of the same runtime sharing
a database, which is not a requirement here). A runtime-supplied parameter for a
fixed value made production code carry a string interpolation (`{p}` in every
query) purely to serve tests.

## Decision

### D1: The namespace is a built-in constant, not a constructor parameter

Each store hard-codes `const NS: &str = "runtime"`. The constructors take no
prefix (`with_pool(pool)`, `connect(url)`, `open(path)`, `open_in_memory()`). One
runtime is one component, so its dispatch and commit tables share the `runtime_`
prefix and one scoped migration ledger (`runtime_schema_migrations`), which is
what keeps the runtime isolated from any *other* component in a shared database.
No deployment configuration is involved.

### D2: Production elegance is not compromised for tests

Test isolation does not justify a production parameter. Parallel Postgres tests
isolate via a **per-test schema** (`CREATE SCHEMA t_x; SET search_path` on the
test pool's `after_connect`), so each test's `runtime_*` tables live in its own
schema. SQLite tests are already isolated (a fresh in-memory or temp-file database
per test). The isolation mechanism lives entirely in the test harness and never
appears in the store's API.

### D3: Do not re-introduce a prefix parameter for "flexibility"

A configured prefix would only be warranted by a *new* requirement — two
deployments of this runtime sharing one database (a tenant axis). That is not a
need today, and YAGNI applies: the component axis is served by the built-in
namespace. If a tenant axis ever arrives, it is a deliberate new decision, not a
parameter kept "just in case".

## Consequences

- The store SQL carries a fixed namespace constant, not a runtime-threaded value;
  production constructors are parameter-free.
- Tests bear 100% of the isolation cost (schema-per-test on Postgres), proven by
  the live suites passing in parallel.
- Adding a prefix/tenant parameter back requires a new ADR motivated by a real
  multi-deployment-one-database requirement.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1/G13 (the commit boundary these stores
  back).
- ADR-0008 — the Postgres commit backend (originally prefix-parameterised).
- ADR-0012 — the SQLite/Postgres store backends over one portable schema.
