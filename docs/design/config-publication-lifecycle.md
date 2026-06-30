# Config Publication Lifecycle

This document makes the config-to-runtime publication lifecycle explicit. The
config domain owns authoring, publication coordination, and registry
compilation. Runtime owns only validation and installation of a complete
published catalog through `RuntimeCatalogInstaller`. Neither side owns the
other's authority.

> **Implemented run input (ADR-0032).** The `compiled` artifact the runtime
> consumes is realized as a single `RunnableConfig` (snapshot + install under one
> fingerprint), built directly or by `compile()`. See
> [config-to-run-execution-flow.md](config-to-run-execution-flow.md#implemented-run-input-runnableconfig-adr-0032).
> The durable `StoredPublication` and the lifecycle below are unchanged.

## Lifecycle States

| State | Owner | Meaning | Next states |
|---|---|---|---|
| `draft` | Config Domain | mutable operator or seed input | `validated`, `rejected` |
| `validated` | Config Domain | schema and local references are valid | `snapshotted`, `rejected` |
| `snapshotted` | Config Domain | source records are frozen with revisions | `compiled`, `rejected` |
| `compiled` | Config Domain / `RegistryCompiler` | registry graph, descriptors, install candidate, and fingerprints are built | `published`, `rejected` |
| `published` | Config Domain | `RegistryPublication` is durable and addressable | `installing`, `superseded` |
| `installing` | Runtime Core adapter / `RuntimeCatalogInstaller` | runtime validates a complete install request and fingerprint | `active`, `rejected` |
| `active` | Runtime Core adapter | registry set is visible to new resolution | `superseded`, `rolled_back` |
| `superseded` | Config Domain and runtime adapter | newer version replaced it | terminal unless rolled back by explicit publication |
| `rolled_back` | Config Domain and runtime adapter | previous known-good publication is active again | `active`, `superseded` |
| `rejected` | owner of failed transition | invalid or conflicting publication | terminal with typed reason |

`ConfigPublicationCoordinator` may orchestrate every transition up to the
runtime install call, but it is not a runtime role. Runtime may install or reject
a publication. It does not edit the source config records that produced it.

## Atomic Install

Installing a runtime catalog is an atomic version swap:

```text
receive RuntimeCatalogInstall
  -> validate publication identity
  -> validate catalog fingerprint
  -> build runtime registry view
  -> swap active version
  -> expose new version to future resolution
```

Active runs keep their effective resolved scope until a safe boundary says
otherwise. Per-step refresh may observe a newer registry version only at an
explicit safe boundary and only through `RunResolver`.

## Failure Rules

| Failure | Required behavior |
|---|---|
| schema invalid | reject before snapshot |
| missing provider/tool/plugin ref | reject before publication or before runtime install |
| descriptor collision | reject publication; do not partially install |
| fingerprint mismatch | runtime install fails closed |
| private admin tool included | reject publication or strip before runtime-visible registry, according to config policy |
| partial install crash | active runtime registry remains the previous complete version |
| rollback requested | publish or activate a complete previous catalog; never mutate history |

## Publication Audit

Each publication should retain:

- publication id and version;
- source config revisions;
- descriptor and catalog fingerprints;
- validation result;
- installer result;
- previous active version;
- rejection reason when applicable.

This data explains runtime behavior without giving runtime config write
authority.

## First Vertical Slice

1. Load two config records into a snapshot.
2. Compile one `RegistryPublication` with a catalog fingerprint through
   `RegistryCompiler`.
3. Install it atomically through `RuntimeCatalogInstaller`.
4. Resolve one run against that version.
5. Reject a conflicting second publication without changing the active version.
6. Roll back by installing a complete previous publication.

## Guardrails

G3, G4, G18, G23, G27, and G29 in [INVARIANTS](../INVARIANTS.md). Stable runtime-facing
roles remain in [runtime-interface-boundaries.md](runtime-interface-boundaries.md#role-catalog).
